//! nest.spool's admission (M8-NEST S1, split from pipes.rs for its budget; a lib.rs module): the shape tests of a plan
//! Material's pull form (PG's lazy Material) and eager form (an invariant, raise-free, non-volatile fill) over the one nest
//! inner that reads it (its reader: spool_readers.rs, D1919), and the refusals' names. Which form keeps the Material's
//! rows is the lifetime home's answer (lifetime/material.rs `spool_fill`). Crate-level through pipes.rs's re-export
//! (graph.rs `source_kind`, nest/inner.rs, source/append.rs, nest/spool.rs read it).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::Am;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::effects::Volatility;
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{Op, Source};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::graph::Gx;

/// D1107 (M8-NEST S1): PG's ExecReScanMaterial discards the store and re-runs the child when a param the child reads changed.
pub(crate) const REFILL_CHILD: &str = "a Material whose child reads an executor parameter (PostgreSQL refills it per changed parameter) (D1107)";
/// D1108: the pull form takes one scan of filters and projections under the Material; the eager form any child that cannot
/// raise and returns the same rows at every rescan.
pub(crate) const LOUD_CHILD: &str = "a Material over a child other than one scan of filters and projections that can raise or differ per rescan (D1108)";
/// D1109: the spool's reader is the one nest (or nested SubPlan) inner that re-opens it.
pub(crate) const MANY_READERS: &str = "a plan Material read by other than one nest inner (D1109)";

/// D1637: a pulled child's site sits inside its reader, so no program pipeline drives its key words (exec admit.rs
/// `late_owner`) and an index key that is an expression binds once at bind; PostgreSQL evaluates it at the child's first
/// scan only (ExecIndexEvalRuntimeKeys), never under an empty outer. nest/spool_site.rs refuses it where B-R1 serves.
pub(crate) const KEY_AT_FIRST_READ: &str = "a pulled Material child whose index key is an expression over no executor parameter (PostgreSQL evaluates it at the child's first scan only) (D1637)";

/// D1630 (its row names it in nest/inner_once.rs, the same text): an inner's walk drives no program pipeline, so exec admit
/// binds its key words once (admit.rs `late_owner`). One level down it is trunk's (DEFERRED to D1630's row); a statement
/// B-R1 newly serves (two or more levels down, or the shift) refuses by this name (spool_readers.rs `walk_bound`).
pub(crate) const WALK_BOUND_AT_BIND: &str = "a nest inner's walk keyed by an expression over no executor parameter (bound once at bind; PG evaluates it at every rescan: D1630)";

/// D1637's class: `s` is an index scan with a key word bound as an expression (source/index.rs: a comparison's value or a
/// row comparison's member that is neither a `Const` nor a bare `Param`; a NULL test binds none). Fail closed: over an
/// executor parameter too (the pull form refuses that by D1107 first).
pub(crate) fn expr_key(s: &Source<'_>) -> bool {
    let bare = |n: Node<'_>| n.as_const().is_some() || n.as_param().is_some();
    let Source::Index { keys, .. } = s else { return false };
    keys.iter().any(|k| match k.form {
        KeyForm::Null | KeyForm::NotNull => false,
        KeyForm::Row => k.value.as_row_compare_expr().is_none_or(|r| !r.rargs.iter().all(bare)),
        _ => !bare(k.value),
    })
}

/// D1484: the pull form reads its child a row at a time outside the frame's window loop; a pgrcolumnar scan's windows,
/// verdicts and covered body live in that loop only (the eager form takes it when the nest's facts allow).
pub(crate) const PGRC_CHILD: &str = "a pulled Material child over a pgrcolumnar scan (its windows and verdicts live in the frame's window loop) (D1484)";

/// D1483 (M8-NEST S2): a union kept in its Materialize form (D455's spool: source/append.rs's feed, emit and ops). PostgreSQL
/// re-runs an Append's arms at every rescan, volatile calls included, so a spool filled once is not its form.
pub(crate) const KEPT_UNION: &str = "a union kept in its Materialize form (PostgreSQL re-runs its arms per rescan: M8-NEST S2) (D1483)";

/// D1486 (M8-NEST S2a): a settled union filled up front is PostgreSQL's Append only when no arm can show it ran (nest/union_arms.rs).
pub(crate) const LOUD_UNION: &str = "a union kept in its Materialize form over an arm that can raise or call a user or VOLATILE function, or an exec-pruned one (PostgreSQL may skip it) (D1486)";

/// M8-NEST S1 (nest.spool's pull form): pipeline `k` fills a plan Material that one nest inner reads and pulls lazily, PG's
/// Material exactly: `Ok(inner)`, the reader's view index (`spool_fill`'s pull form). The inner makes the child's source and
/// ops again (nest/spool.rs) and `k` itself reads nothing (graph.rs `source_kind`). `Err` names why not (`Err("")`: `k`
/// fills no plan Material).
pub(crate) fn spool_pull(gx: &Gx<'_, '_>, k: usize) -> Result<usize, &'static str> {
    match crate::lifetime::material::spool_fill(gx, k)? {
        (r, true) => Ok(r),
        (_, false) => Err(LOUD_CHILD),
    }
}

/// D3498: fill pipeline `p` (its ops before the Spool, `body`) is a Result's one row: no input, one-time gates, filters
/// and projections reading no executor parameter (no set function or join). PostgreSQL's Material over it makes that row,
/// every call and raise of it, at its first fetch and keeps it, so its pull form (ops/material/lower.rs `make_reader`) is
/// exact whatever the row does.
pub(crate) fn result_child(p: &gravity_decompose::PipelineNode<'_>, body: &[Op<'_>]) -> bool {
    matches!(p.source, Source::Const)
        && p.desc.consumes.is_empty()
        && body.iter().all(|op| match op {
            Op::Filter(q) => !exec_param(*q),
            // a gate is the Result's resconstantqual, an implicit-AND list
            Op::Gate(g) => g.as_list().map_or_else(|| !exec_param(*g), |l| !l.iter().any(exec_param)),
            Op::Project(l) => !l.iter().any(exec_param),
            _ => false,
        })
}

/// D1108 row 3: facts no other eager arm of `spool_fill` takes (the effects home's `deferred_fill`), over a child that is
/// not constants only; also (D3373; D3297's acting statement among them) errorhold's licensed facts where its arm
/// cannot take them (`licensed_arm`: the plan-order re-run, where the licence is denied, a statement that acts, as #424's
/// UPDATE does, a fill that calls user code or a reader two levels down under a walk bound at bind): run whole at the
/// nest's first outer row, PostgreSQL's own order, they need no licence.
pub(crate) fn deferred_child(gx: &Gx<'_, '_>, k: usize, o: usize, inv: bool, rf: bool, vol: Volatility) -> bool {
    let licensed = crate::effects::licensed_fill(inv, rf, vol) && !licensed_arm(gx, k, o, inv, rf, vol);
    (crate::effects::deferred_fill(inv, rf, vol) || licensed) && !crate::effects::visible::material::quiet(gx, k)
}

/// errorhold (D1108 row 2)'s arm of `spool_fill`: fill pipeline `k`, read by a nest of pipeline `o`, of licensed facts
/// (invariant, IMMUTABLE, may raise) runs ahead in pipeline order under the recheck licence: its reader a one-level nest,
/// or one two or more levels down when no inner of the graph walks by a word bound once at bind (D1630's guard), the
/// licence available (`licensable`) and no user code in the fill (D2573).
pub(crate) fn licensed_arm(gx: &Gx<'_, '_>, k: usize, o: usize, inv: bool, rf: bool, vol: Volatility) -> bool {
    crate::effects::licensed_fill(inv, rf, vol)
        && (o < gx.pipelines().len() || !crate::rerun::spool::readers::walk_bound(gx))
        && licensable(gx)
        && !crate::effects::fed_user_code(gx, k)
}

/// `DECISIONS.md` 4 (D) b, `spool_fill`'s last arm: fill pipeline `k`, read by a nest of pipeline `o`, of facts that may
/// raise (invariant, so one fill serves every rescan, and not VOLATILE) that no arm above takes, with no user code in the
/// fill and its reader a one-level nest or one under no walk bound at bind (D1630's guard, as `licensed_arm`), fills up
/// front with no recheck when `licence_unchecked` grants it for the statement (acting statements are denied), asked as
/// user code when the fill holds what the side class does not look inside (`effects::hides_user_code`, D3566). In
/// errorhold's plan-order run it takes a fill of errorhold's licensed facts too (its first run raised under the recheck
/// licence): the run answers that error, tagged, where PostgreSQL may never fill the Material (an empty outer reads no
/// inner row; `DECISIONS.md` 4 (D) b, a data error from rows PostgreSQL's plan does not read) and where PostgreSQL fills it
/// and raises the same error. Not when the reader nest is inside an embedded inner (`o` past the graph's pipelines): there
/// the plan-order run refuses by D1108's name.
pub(crate) fn unchecked_arm(gx: &Gx<'_, '_>, k: usize, o: usize, inv: bool, rf: bool, vol: Volatility) -> bool {
    use gravity_decompose::effects::{licence_unchecked, Effects};
    crate::effects::unchecked_fill(inv, rf, vol)
        && !(gravity_decompose::effects::denying() && crate::effects::licensed_fill(inv, rf, vol) && o >= gx.pipelines().len())
        && (o < gx.pipelines().len() || !crate::rerun::spool::readers::walk_bound(gx))
        && !crate::effects::fed_user_code(gx, k)
        && licence_unchecked(Effects::RAISES, crate::effects::hides_user_code(gx, k)).is_ok()
}

/// `spool_fill`'s arms of D1108 row 3 (the fill run whole at the nest's first outer row, PostgreSQL's own order) for fill
/// pipeline `k` read at op `j` of pipeline `o`.
fn deferred_form(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize, many: bool, (inv, rf, vol): (bool, bool, Volatility)) -> bool {
    deferred_child(gx, k, o, inv, rf, vol) && (whole_first(gx, k, o, j) || many && whole_first_arms(gx, k, o, j) || !many && (lock_fill(gx, k, o, j) || held_child(gx, k, o, j) || first_pull(gx, k, o, j) || first_sort(gx, k, o, j)))
}

/// D1108 row 3: fill pipeline `k` takes the eager form by `whole_first` alone, so lazy.rs must defer it to the nest's first
/// outer row or refuse.
pub(crate) fn deferred(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return false };
    let (o, j) = ps.inners()[reader - ps.len()].0;
    o < ps.len() && matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if deferred_child(gx, k, o, inv, rf, vol))
}

/// D1108 row 3: PostgreSQL's Material over fill pipeline `k` runs its child once, whole, inside the first outer row's inner
/// scan of the nest at op `j` of pipeline `o`, and keeps it: the nest is the root pipeline's, INNER or LEFT, not unique, with
/// no parameters (EXEC_FLAG_REWIND: ExecReScanMaterial rewinds the store), and the child (the fill and its producers)
/// reads no executor parameter, so no rescan refills it. Run whole there (lazy.rs), it shows what PostgreSQL shows when
/// nothing between it and the client stops the scan or shows the interleaving: the outer side up to the nest, its qual and
/// the ops after it are quiet filters and projections into the client's sink, no read stops at a count (a cursor's FETCH
/// n), nothing writes or locks, and no rerun set re-runs `o`.
/// material_values_quiet :54: or the root is an INSERT .. SELECT's, those quiet ops carrying the nest's rows into its
/// write, whose per-row work calls nothing (`insert_at`; the write is then the one thing that acts).
/// D3373: or the nest's rows go, through quiet filters and projections, into a full Sort or a plain aggregate
/// that `o` ends in and PostgreSQL surely runs (merge.join's lazy_outer.rs `reached`: one reader of it, every pipeline
/// between quiet, on to the root's client): ExecSort / ExecAgg read every row the nest makes before any row leaves them,
/// so nothing after the nest stops its scan early or shows the interleaving (the root's count(*), ORDER BY, GROUP BY over
/// the nest).
pub(crate) fn whole_first(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_decompose::effects::denying_bounded;
    use types_nodes::jointype::JoinType;
    let ps = gx.pipelines();
    let Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, params, qual, unique: false, .. })) = ps[o].ops.get(j) else { return false };
    let into = |to: usize| ps[o].ops[j + 1..to].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && crate::effects::span_quiet(gx, &ps[o], j + 1, to, false);
    let end = ps[o].ops.len();
    let root = o == usize::from(gx.graph.root);
    let write = if root { insert_at(gx, k, &ps[o].ops) } else { None };
    let tail = if root {
        into(write.unwrap_or(end))
    } else {
        let to = if crate::ops::mergejoin::lazy_outer::full(&ps[o]) { end - 1 } else { end };
        // a graph pipeline (lazy.rs defers no fill for a nest inside an inner: its owner's call runs it)
        o < ps.len() && to > j && into(to) && crate::ops::mergejoin::lazy_outer::reached(gx, o, j + 1)
    };
    let acts = ps.iter().enumerate().any(|(i, p)| p.ops.iter().enumerate().any(|(x, op)| matches!(op, Op::Write(_) | Op::Lock(_)) && (i, Some(x)) != (o, write)));
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let child = crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src));
    params.is_nil()
        && tail
        && !acts
        && !denying_bounded()
        && qual.iter().all(|n| crate::effects::expr_quiet(gx, n))
        && crate::effects::span_quiet(gx, &ps[o], 0, j, false)
        && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(o as u16)))
        && core::iter::once(k).chain(child).all(|x| !reads_param(&ps[x]))
}

/// D3718 (LOCK-FILL): fill pipeline `k` takes its own row locks, a LIMIT .. FOR UPDATE subquery PostgreSQL plans apart
/// under the Material (Materialize -> Subquery Scan -> Limit -> LockRows -> scan, the statement's own LockRows holding only
/// REFERENCE marks), read by the nest at op `j` of the root pipeline `o`. PostgreSQL's Material pulls that child a row per
/// fetch as the nest's first outer row reads its inner (an INNER or LEFT nest that is not unique and has no parameters
/// reads its inner to the end there) and rewinds its store at every later row: LockRows' epqParam is set and read below it
/// only (finalize_plan's locally added param), so no rescan refills it. Its locks are taken once, during that row, never
/// under an empty outer, each before the rows joined to the row it locks are sent (D4204), and its EvalPlanQual rechecks
/// the child's own scan alone. Pulled so by its reader (`lock_fill_pulls`), or else run whole at that row (lazy.rs), the
/// same rows lock in the same order. The outer side before the nest may lock its own rows too (the other subquery's
/// Limit -> LockRows): each outer row is locked, counted and handed to the nest before the next is read, so the fill runs
/// after the first outer row's lock and before the second's, PostgreSQL's order. Nothing else locks (a lock above the
/// nest re-runs the fill in its EvalPlanQual) and nothing writes; the fill is a heap scan of quiet keys, quiet filters
/// and projections, its locks and constant limits, feeding on nothing; the outer side before the nest the same over its
/// source; the join qual and the ops after the nest quiet filters and projections; no rerun set re-runs the fill or the
/// nest; no bounded read.
pub(crate) fn lock_fill(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_decompose::effects::denying_bounded;
    use gravity_decompose::graph::Bound;
    use types_nodes::jointype::JoinType;
    let ps = gx.pipelines();
    if k >= ps.len() || o != usize::from(gx.graph.root) || o >= ps.len() {
        return false;
    }
    let Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, params, qual, unique: false, .. })) = ps[o].ops.get(j) else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = ps[k].ops.split_last() else { return false };
    let quiet = |n: Node<'_>| crate::effects::expr_quiet(gx, n) && !exec_param(n);
    // a scan of quiet keys; its ops quiet filters and projections, row locks and constant LIMIT / OFFSET bounds
    let source = |src: &Source<'_>| match src {
        Source::Range { am: Am::Heap, .. } => true,
        Source::Index { am: Am::Heap, keys, recheck, .. } => keys.iter().all(|x| quiet(x.value)) && recheck.iter().all(quiet),
        _ => false,
    };
    let fixed = |b: &Bound<'_>| !matches!(b, Bound::Expr(_));
    let side = |ops: &[Op<'_>]| {
        ops.iter().all(|op| match crate::frame::iface::demand(op) {
            Some(d) => !d.ties && fixed(d.count) && fixed(d.offset),
            None => match op {
                Op::Filter(n) | Op::Gate(n) => quiet(*n),
                Op::Project(l) => l.iter().all(quiet),
                Op::Lock(_) => true,
                _ => false,
            },
        })
    };
    let locks_in = |ops: &[Op<'_>]| ops.iter().any(|op| matches!(op, Op::Lock(_)));
    // every lock is the fill's or the outer side's before the nest (no nest inner or SubPlan locks), and nothing writes
    let placed = (0..ps.len()).all(|x| {
        ps[x].ops.iter().enumerate().all(|(i, op)| match op {
            Op::Write(_) => false,
            Op::Lock(_) => x == k || x == o && i < j,
            Op::Join(JoinDesc::Nest { inner, .. }) => !crate::lifetime::material::locks(inner),
            Op::SubPlan(d) => !matches!(&d.form, SubForm::Nested { inner } if crate::lifetime::material::locks(inner)),
            _ => true,
        })
    });
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let tail = ps[o].ops[j + 1..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && crate::effects::span_quiet(gx, &ps[o], j + 1, ps[o].ops.len(), false);
    params.is_nil()
        && locks_in(body)
        && placed
        && source(&ps[k].source)
        && ps[k].desc.consumes.is_empty()
        && side(body)
        && side(&ps[o].ops[..j])
        && (source(&ps[o].source) || matches!(ps[o].source, Source::Breaker { .. } | Source::Const) && !locks_in(&ps[o].ops[..j]))
        && qual.iter().all(|n| crate::effects::expr_quiet(gx, n))
        && tail
        && !denying_bounded()
        && !rerun(k)
        && !rerun(o)
}

/// D3718: pipeline `q` fills a plan Material in `lock_fill`'s form (its one reader the root's nest, `spool_fill`'s arm
/// through it, pulled or run whole): lockrows' M4 pair takes its lock as A (ops/lockrows/plan.rs `Plan::role`).
pub(crate) fn lock_fill_of(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, _)) = crate::lifetime::material::spool_fill(gx, q) else { return false };
    if reader < ps.len() || !matches!(crate::rerun::spool::readers::reader_or_copies(gx, q), Ok((_, false))) {
        return false;
    }
    let (o, j) = ps.inners()[reader - ps.len()].0;
    o < ps.len() && lock_fill(gx, q, o, j)
}

/// D4204: `lock_fill`'s child pulled one row per fetch by its reader (ops/material/lower.rs `PulledLimit`), as
/// PostgreSQL's ExecMaterial pulls it: each row locked when the nest reaches it, so the rows joined before it are sent
/// first. Taken when the child holds at most one LIMIT / OFFSET, above every lock, whose bounds are no negative constant
/// (ExecLimit's error at the first fetch); else the child runs whole at the first outer row, as before.
pub(crate) fn lock_fill_pulls(gx: &Gx<'_, '_>, k: usize) -> bool {
    use crate::frame::iface::{demand, limit_settled};
    let ops = &gx.pipelines()[k].ops;
    let mut limits = ops.iter().enumerate().filter(|(_, o)| demand(o).is_some());
    let last_lock = ops.iter().rposition(|o| matches!(o, Op::Lock(_)));
    match (limits.next(), limits.next()) {
        (None, _) => true,
        (Some((i, o)), None) => last_lock.is_some_and(|l| l < i) && limit_settled(o) && demand(o).is_some_and(|d| !d.ties),
        _ => false,
    }
}

/// D4204: fill pipeline `q` is `lock_fill`'s child in the pull form (`lock_fill_pulls`).
pub(crate) fn lock_fill_pulled(gx: &Gx<'_, '_>, q: usize) -> bool {
    matches!(crate::lifetime::material::spool_fill(gx, q), Ok((_, true))) && lock_fill_of(gx, q)
}

/// material_values_quiet :54 (`whole_first`): the index of the INSERT that root ops `ops` end in (its RETURNING
/// projection after it), when fill pipeline `k` calls no user code and the write's per-row work is ModifyTable's alone,
/// calling nothing (union_arms.rs `insert_still`'s reading): no ON CONFLICT and no check option, a RETURNING list that
/// calls no user or VOLATILE function and runs no SubPlan, and no BEFORE or INSTEAD OF row trigger for INSERT on the target
/// or a routed leaf. ExecModifyTable inserts each row the nest hands it before it asks for the next, so PostgreSQL's
/// Material fill runs between the first outer row's inserts; but the fill reads under the command's snapshot (the inserted
/// rows stay invisible to it whenever it runs) and the inserts call nothing, so with no error the rows written and
/// returned are PostgreSQL's. Only the order of two errors can differ (an insert's constraint error before the error a
/// later fill row raises), as rule B's INSERT has it.
fn insert_at(gx: &Gx<'_, '_>, k: usize, ops: &[Op<'_>]) -> Option<usize> {
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_INSERT};
    use gravity_program::Cmd;
    let calls = |n: Node<'_>| crate::effects::user_or_volatile(gx, n) || gravity_decompose::expr::has_subplan(n);
    let (at, w) = match ops {
        [.., Op::Write(w)] => (ops.len() - 1, w),
        [.., Op::Write(w), Op::Project(l)] if w.returning.is_some() && !l.iter().any(calls) => (ops.len() - 2, w),
        _ => return None,
    };
    let still = |t: TrigExt| t.known && (t.before_row | t.instead_row) & TRIG_INSERT == 0;
    let leaves = w.route.is_none() || w.leaves_known && w.leaves.iter().all(|l| still(l.trig));
    (w.cmd == Cmd::Insert
        && w.on_conflict.is_none()
        && w.merge.is_none()
        && w.checks.is_empty()
        && w.returning.is_none_or(|l| !l.iter().any(calls))
        && still(gx.facts.trig_ext(w.plan.target_rel))
        && leaves
        && !crate::effects::fed_user_code(gx, k))
    .then_some(at)
}

/// rowsecurity#343: `whole_first` for a fill read by the nest copies of a several-target write's streamed arms
/// (readers.rs `copies`). PostgreSQL's one Nested Loop takes the arms' rows in order and its Materialize runs the child
/// once, whole, inside the first outer row's inner scan, whichever arm that row comes from; later scans rewind. Run whole
/// there (lazy.rs: at the first wait of any arm), it shows PostgreSQL's rows, calls and errors when `o` is an arm of the
/// root's streamed union, the nest INNER or LEFT, not unique, with no parameters and a quiet qual; the arm's ops after the
/// nest are filters, projections and the write, calling no user or VOLATILE function and running no SubPlan (each may
/// raise: a raise mid-scan stops PostgreSQL's fill early, error against error), the
/// write ModifyTable's per-row work alone (`write_calm`; the scans read under the command's snapshot, so the write's rows
/// stay invisible to the fill whenever it runs); nothing else writes or locks; no cursor stops at a count; no rerun set
/// re-runs an arm; the child reads no executor parameter but a fixed, unseen InitPlan's (D3297, `init_read`: PostgreSQL's
/// Material never refills over it), and a settled union in it prunes at run time over such InitPlans' parameters only
/// (`init_prune`: PostgreSQL's Append decides it at its first call, inside that first inner scan, where the run whole
/// decides it too). The outer side up to the nest may call: the fill runs at the
/// nest step's first row, after the calls of the rows before it and before those after it, PostgreSQL's order.
pub(crate) fn whole_first_arms(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_decompose::effects::denying_bounded;
    use types_nodes::jointype::JoinType;
    let ps = gx.pipelines();
    let Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, params, qual, unique: false, .. })) = ps[o].ops.get(j) else { return false };
    let root = usize::from(gx.graph.root);
    let streamed = gx.stream_arms(root);
    let arms = || streamed.iter().copied().chain(core::iter::once(root));
    let calls = |n: Node<'_>| crate::effects::user_fn(gx, n) || crate::effects::volatile(gx, n) || gravity_decompose::expr::has_subplan(n);
    let after = ps[o].ops[j + 1..].iter().all(|op| match op {
        Op::Filter(n) => !calls(*n),
        Op::Project(l) => !l.iter().any(calls),
        Op::Write(w) => crate::effects::visible::rerun::write_calm(gx, w, &calls),
        _ => false,
    });
    let acts = (0..ps.len()).any(|i| ps[i].ops.iter().any(|op| matches!(op, Op::Lock(_)) || matches!(op, Op::Write(_)) && !arms().any(|a| a == i)));
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let child = crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src));
    params.is_nil()
        && arms().any(|a| a == o)
        && after
        && !acts
        && !denying_bounded()
        && qual.iter().all(|n| crate::effects::expr_quiet(gx, n))
        && !gx.graph.reruns.iter().any(|r| arms().any(|a| r.pipelines.contains(&(a as u16))))
        && core::iter::once(k).chain(child).all(|x| {
            // D3714: or over InitPlans that may raise (`late_param`), which the run whole decides at its first arm's
            // start and whose parameters the arms read after it, as PostgreSQL's first Append call runs them
            let loud: &dyn Fn(&PruneDesc<'_>) -> bool = &|d| d.exec && !init_prune(gx, x, d) && late_param(gx, x, d).is_none();
            !reads_param_in(&ps[x], Some(loud), &|n| exec_param(n) && !init_read_by(n, &|id| init_late(gx, id)))
        })
}

/// D1108 (rxshape1, op 14 "held at len"): fill pipeline `k` of the plan Material the nest at op `j` of graph pipeline `o`
/// reads (its one reader) runs whole at the nest's first outer row (lazy.rs, as `whole_first`'s fill) and an error it
/// raises is held at the record it stopped at: the fill's merge defers it (the program marks `k` `held_sort`, exec's
/// ops/sort/held.rs stamps the Material slot's region `RUN_HELD`) and the reader raises it when it reaches that record
/// (ops/material/lower.rs, kernels `m11_run_held`), where PostgreSQL's Material asks its child for the row whose making
/// raises (ExecMaterial reads its child one row per call, the store's end first). Exact when the fill's rows come in
/// PostgreSQL's order, each made as PostgreSQL's child makes it when asked for the next row, and nothing it does but its
/// error shows (`lazy_exact`): then the records before the stop are PostgreSQL's rows, the reader meets the stop where
/// PostgreSQL's child raises, a reader that never reaches it (a SEMI or ANTI match, a LIMIT, a cursor stopping early, an
/// empty outer) never raises, and between the rows the reader reads nothing the fill would have done shows. Its reader is
/// one nest, no rerun set re-runs the fill or the nest (D1107's refill; a rerun call's held layer), the child reads no
/// executor parameter, nothing feeds it but a quiet plan Material it reads (effects `store_quiet`: its feeders would run outside
/// the hold; a quiet one shows nothing there) and nothing in the statement writes or locks (EvalPlanQual's recheck re-runs
/// a fill).
/// D1108: the reader nest may be inside an embedded inner (`o` past the graph's
/// pipelines, errorhold_d1108_two_level's Nested Loop -> inner Nested Loop -> Materialize). lazy.rs defers no fill there
/// (its owner's call runs the nest), so the fill runs in pipeline order, serially in heap order (`held_serial`), its error
/// held all the same: the reader raises it at the record the fill stopped at, and a reader that never gets there (an
/// empty outer at either level) never raises. D1630's guard as errorhold's arm: no inner of the graph walks by a word
/// bound once at bind (`readers::walk_bound`).
pub(crate) fn held_child(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    if k >= ps.len() || gx.graph.acts || ps.iter().any(crate::lifetime::material::locks) {
        return false;
    }
    if o >= ps.len() && crate::rerun::spool::readers::walk_bound(gx) {
        return false;
    }
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let Some(Op::Join(JoinDesc::Nest { .. })) = ps[o].ops.get(j) else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = ps[k].ops.split_last() else { return false };
    !rerun(k) && !rerun(owner(gx, o)) && exact_child(gx, &ps[k].source, &ps[k].desc.consumes, Some(k), body)
}

/// The graph pipeline whose call runs pipeline `o`: `o` itself, or the owner an embedded inner's chain of owners ends in.
fn owner(gx: &Gx<'_, '_>, mut o: usize) -> usize {
    let ps = gx.pipelines();
    while o >= ps.len() {
        o = ps.inners()[o - ps.len()].0 .0;
    }
    o
}

/// The pipelines a held fill `k` (`held`) whose reader nest is inside an embedded inner runs serially, on the leader:
/// the fill and the pipelines feeding it (effects `store_quiet`'s Materials), so its rows, and the record its error stops at,
/// are PostgreSQL's heap order at every width. Empty for a one-level reader: lazy.rs runs that fill at the nest's first
/// outer row, at width 1.
pub(crate) fn held_serial<'g>(gx: &Gx<'g, '_>, k: usize) -> mcx::PgVec<'g, usize> {
    use mcx::GrowIn;
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return mcx::PgVec::new_in(gx.mcx) };
    if ps.inners()[reader - ps.len()].0 .0 < ps.len() || held(gx, k).is_none() {
        return mcx::PgVec::new_in(gx.mcx);
    }
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let mut out = mcx::pg_vec![in gx.mcx; k];
    out.extend_in(crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src)));
    out
}

/// `held_child`'s child: a source and ops whose rows come one at a time in PostgreSQL's order, each op run on a row where
/// PostgreSQL's node runs it when its parent asks for the next row, calling no user or VOLATILE function, running no
/// SubPlan and reading no executor parameter (calm: what they do shows only as an error, and every rescan of the
/// Material reads the same rows: no refill, D1107). A heap scan (its quals per row, an index scan's keys at its first
/// row), a FROM function's one row or a VALUES list (each row's expressions as it is fetched); filters, one-time gates and
/// projections; a FROM series or unnest call (it raises at its first call only, where PostgreSQL's FunctionScan, reading
/// it whole there, raises too); a constant LIMIT; a nest over an inner of the same form, nothing feeding it.
pub(crate) fn lazy_exact(gx: &Gx<'_, '_>, source: &Source<'_>, consumes: &[u8], ops: &[Op<'_>]) -> bool {
    exact_child(gx, source, consumes, None, ops)
}

/// `lazy_exact`, and for `held_child` (`fill` = the fill pipeline) a nest inner or source reading a quiet plan Material
/// (effects `store_quiet`) as well: its records come in its fill's order, the same at every read, and reading them shows nothing.
fn exact_child(gx: &Gx<'_, '_>, source: &Source<'_>, consumes: &[u8], fill: Option<usize>, ops: &[Op<'_>]) -> bool {
    crate::stack_check();
    use gravity_decompose::ops::shared::srf::{SrfKind, SrfPlace};
    let calm = |n: Node<'_>| !exec_param(n) && !crate::effects::user_or_volatile(gx, n) && !gravity_decompose::expr::has_subplan(n);
    let list = |l: &types_nodes::list::NodeList<'_>| l.iter().all(calm);
    let store = |s: u8| fill.is_some_and(|k| crate::effects::visible::material::store_quiet(gx, k, s));
    let src = match source {
        Source::Range { am: Am::Heap, .. } | Source::Const => true,
        Source::Index { am: Am::Heap, keys, recheck, .. } => keys.iter().all(|x| calm(x.value)) && list(recheck),
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(list)),
        Source::Breaker { slot } => store(*slot),
        _ => false,
    };
    let bound = |b: &gravity_decompose::graph::Bound<'_>| match b {
        gravity_decompose::graph::Bound::Expr(n) => calm(*n),
        _ => true,
    };
    src && consumes.iter().all(|&s| store(s))
        && ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => calm(*n),
            Op::Project(l) => list(l),
            Op::Srf(d) => {
                matches!(d.place, SrfPlace::From { .. })
                    && d.functions.iter().all(|f| matches!(f.kind, SrfKind::SeriesInt { .. } | SrfKind::Unnest) && list(f.args))
                    && list(d.tlist)
            }
            Op::Join(JoinDesc::Nest { params, qual, inner, .. }) => params.is_nil() && list(qual) && exact_child(gx, &inner.source, &inner.desc.consumes, fill, &inner.ops),
            op => crate::frame::iface::limit_bounds(op).is_some_and(|(count, offset)| bound(count) && bound(offset)),
        })
}

/// Fill pipeline `k` takes `spool_fill`'s held form (`held_child`): its deferred facts where `whole_first` does not run it
/// whole (a reader that may stop before the fill's end). The fill runs whole at the first outer row, past the reader's
/// stop, its error held at the record the reader reaches: the held error's licence (`held_child`: the statement acts
/// nowhere and the fill calls no user or VOLATILE function), None where the form is not taken.
pub(crate) fn held(gx: &Gx<'_, '_>, k: usize) -> Option<gravity_decompose::effects::Licence> {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return None };
    if !matches!(crate::rerun::spool::readers::reader_or_copies(gx, k), Ok((_, false))) {
        return None;
    }
    let (o, j) = ps.inners()[reader - ps.len()].0;
    let taken = matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if deferred_child(gx, k, o, inv, rf, vol))
        && !whole_first(gx, k, o, j)
        && held_child(gx, k, o, j);
    taken.then(|| gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, false, None).ok()).flatten()
}

/// D1108 (rxshape1, first pull): PostgreSQL's Material over fill pipeline `k` read by the nest at op `j` of graph pipeline
/// `o` does all its child's work at its first fetch, inside the first outer row's inner scan, whatever reads after it: the
/// child is a hashed or plain Agg (nodeAgg.c reads its whole input there: agg_fill_hash_table, agg_retrieve_direct's one
/// group; a hash table past hash_mem spills input whose arguments run at later fetches, which this form does not model),
/// every aggregate's final quiet (`final_quiet`; a hashed Agg finalizes group by group as it hands rows on) and no
/// grouping set or two-pass form, and the fill's own ops (the HAVING qual and target list, run per row as the Material
/// asks) quiet. Run whole at the nest's first outer row (lazy.rs, as `whole_first`'s fill), it raises, calls and moves
/// what PostgreSQL's first fetch does, in its order, and a reader that stops early (a unique or SEMI inner, a LIMIT, a
/// cursor) or an empty outer changes nothing: PostgreSQL's later fetches read the store only. Its reader is one nest of
/// a graph pipeline without parameters, no rerun set re-runs the fill or the nest (D1107), the child reads no executor
/// parameter and nothing in the statement writes or locks (a sequence's nextval in an argument moves where PostgreSQL's
/// first fetch moves it; the statement acting keeps errorhold's licence away, so no re-run repeats it).
pub(crate) fn first_pull(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_decompose::ops::shared::agg::GroupForm;
    let ps = gx.pipelines();
    let acts = ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    if k >= ps.len() || o >= ps.len() || acts || ps.iter().any(crate::lifetime::material::locks) {
        return false;
    }
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let Some(Op::Join(JoinDesc::Nest { params, .. })) = ps[o].ops.get(j) else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = ps[k].ops.split_last() else { return false };
    let Source::Breaker { slot } = ps[k].source else { return false };
    let Some(life) = gx.life_of(k, slot) else { return false };
    let Some(g) = gx.graph.groups.iter().find(|g| g.producer == life) else { return false };
    // an aggregate's value (its arguments ran in the transition, at the first fetch), or an expression over no aggregate
    // that raises and calls nothing
    let still = |n: Node<'_>| {
        let n = n.as_target_entry().map_or(n, |t| t.expr);
        let bare = gravity_decompose::expr::walk(n, &mut |x| x.as_aggref().is_none());
        n.as_aggref().is_some() || bare && crate::effects::expr_quiet(gx, n) && !crate::effects::user_or_volatile(gx, n) && !gravity_decompose::expr::has_subplan(n)
    };
    let quiet = |op: &Op<'_>| match op {
        Op::Filter(n) => still(*n),
        Op::Project(l) => l.iter().all(still),
        _ => false,
    };
    let child = crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(Some(slot)));
    params.is_nil()
        && !rerun(k)
        && !rerun(o)
        && matches!(g.form, GroupForm::Hashed | GroupForm::Plain)
        && g.two_pass.is_none()
        && g.set.is_none()
        && g.aggs.iter().all(crate::effects::visible::agg::final_quiet)
        && body.iter().all(quiet)
        && ps[k].desc.consumes.iter().all(|&s| s == slot)
        && core::iter::once(k).chain(child).all(|x| !reads_param(&ps[x]))
}

/// D4102 (first sort): PostgreSQL's Material over fill pipeline `k`, whose child is a plan Sort node (ExecSort; a top-N
/// one under a LIMIT too; the fill's source, not a sort decompose adds) over work that may raise or call, does all that
/// work at its first fetch, inside the first outer row's inner scan of the nest at op `j` of graph pipeline `o`: the Sort
/// reads its whole input before it hands on its first row.
/// Past the Sort's run, the fill's own ops are a LIMIT / OFFSET whose bounds cannot raise and quiet filters and
/// projections (no call, no SubPlan, no raise). So a reader that stops early (a unique or SEMI inner, a LIMIT, a cursor)
/// or an empty outer changes nothing: PostgreSQL's later fetches read the store only. Run whole at the nest's first outer
/// row (lazy.rs, as `first_pull`'s fill), it raises and calls what PostgreSQL's first fetch does. Its reader is one nest
/// without parameters, nothing in the statement writes or locks, and the fill and every pipeline that feeds it read no
/// executor parameter and sit in no rerun set. The nest's pipeline sits in no rerun set, or in a recursive union's
/// generation set alone (RECURSIVE_SET): the term's rescan changes only the work table's param (ExecRecursiveUnion), and
/// an inner with no nest params is asked to rewind (ExecInitNestLoop's EXEC_FLAG_REWIND), so ExecReScanMaterial keeps the
/// store and the child runs once, at the first fetch of the first generation whose nest reaches it. The fill's lazy
/// group runs at that wait, and later generations read its record.
pub(crate) fn first_sort(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    use gravity_decompose::ops::shared::setop::RECURSIVE_SET;
    let ps = gx.pipelines();
    let acts = ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    if k >= ps.len() || o >= ps.len() || acts || ps.iter().any(crate::lifetime::material::locks) {
        return false;
    }
    let Some(Op::Join(JoinDesc::Nest { params, .. })) = ps[o].ops.get(j) else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = ps[k].ops.split_last() else { return false };
    let Source::Breaker { slot } = ps[k].source else { return false };
    let Some(life) = gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&l| l < ps.len()) else { return false };
    if !matches!(crate::frame::iface::end_breaker(gx, life), Some(BreakerKind::Sort | BreakerKind::TopK)) {
        return false;
    }
    // the sort is the plan's own Sort node, the fill's source: a sort decompose puts over a node that streams (a sorted
    // grouping sets' order over its hashed form) runs its input whole where PostgreSQL's node hands on rows as it goes
    let plan_sort = gx.graph.stmt.zip(ps[k].node_ops.first()).and_then(|(s, n)| gravity_decompose::lifetime::node_of(s, n.node_id));
    if !plan_sort.is_some_and(|(n, _)| n.node_tag() == types_nodes::NodeTag::T_Sort) {
        return false;
    }
    let still = |n: Node<'_>| {
        let n = n.as_target_entry().map_or(n, |t| t.expr);
        crate::effects::expr_quiet(gx, n) && !crate::effects::user_or_volatile(gx, n) && !gravity_decompose::expr::has_subplan(n)
    };
    let quiet = |op: &Op<'_>| match op {
        Op::Filter(n) => still(*n),
        Op::Project(l) => l.iter().all(still),
        _ => crate::effects::visible::quiet_limit(op),
    };
    let sets = |x: usize| gx.graph.reruns.iter().filter(move |r| r.pipelines.contains(&(x as u16)));
    let term = sets(o).count() <= 1 && sets(o).all(|r| r.plan_id == RECURSIVE_SET);
    let child = crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(Some(slot)));
    params.is_nil()
        && term
        && body.iter().all(quiet)
        && ps[k].desc.consumes.iter().all(|&s| s == slot)
        && core::iter::once(k).chain(child).all(|x| sets(x).next().is_none() && !reads_param(&ps[x]))
}

/// D3803 (decompose nest.selfspool's tree form; a plan Material over such a child too): PostgreSQL's Material over fill
/// pipeline `k`, read by the nest at op `j` of graph pipeline `o`, does at its first fetch, inside the first outer row's
/// inner scan, all of its child's work that may show: a FROM function over the Result (ExecMakeTableFunctionResult runs it
/// to its end into the FunctionScan's tuplestore at its first fetch, VOLATILE or raising as it is) and the builds its hash
/// joins probe (their placement is the hash join's own, ops/hashjoin), whatever reads after it. The rest of the child is
/// per-row work run as far as the Material is read: a heap scan (an index scan's keys quiet), the FROM function's quiet
/// target list and the effects home's `rows_quiet` ops (filters, projections, hash probes, nests over index lookups).
/// Run whole at the nest's first outer row (lazy.rs, as `first_pull`'s fill), it calls and raises what PostgreSQL's first
/// fetch does, in its order, and a reader that stops early (a SEMI or ANTI match, a unique inner, a LIMIT, a cursor) or
/// an empty outer changes nothing: the per-row work shows nothing, and PostgreSQL's later fetches read its store. Its
/// reader is one nest of a graph pipeline without parameters, no rerun set re-runs the fill or the nest (D1107), the
/// child and its producers read no executor parameter and nothing in the statement writes or locks.
pub(crate) fn first_run(gx: &Gx<'_, '_>, k: usize, o: usize, j: usize) -> bool {
    use gravity_decompose::ops::shared::srf::SrfPlace;
    let ps = gx.pipelines();
    let acts = ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    if k >= ps.len() || o >= ps.len() || acts || ps.iter().any(crate::lifetime::material::locks) {
        return false;
    }
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let Some(Op::Join(JoinDesc::Nest { params, .. })) = ps[o].ops.get(j) else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = ps[k].ops.split_last() else { return false };
    let quiet = |n: Node<'_>| crate::effects::expr_quiet(gx, n) && !exec_param(n);
    let list = |l: &types_nodes::list::NodeList<'_>| l.iter().all(quiet);
    let (from, rest) = match (body.split_first(), &ps[k].source) {
        (Some((Op::Srf(d), rest)), Source::Const) if matches!(d.place, SrfPlace::From { .. }) && d.functions.iter().all(|f| !f.args.iter().any(exec_param)) && list(d.tlist) => (true, rest),
        _ => (false, body),
    };
    let src = from
        || match &ps[k].source {
            Source::Range { am: Am::Heap, .. } => true,
            Source::Index { am: Am::Heap, keys, recheck, .. } => keys.iter().all(|x| quiet(x.value)) && list(recheck),
            _ => false,
        };
    let per_row = crate::effects::visible::material::rows_quiet(gx, rest);
    let child = crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied());
    params.is_nil() && !rerun(k) && !rerun(o) && src && per_row && core::iter::once(k).chain(child).all(|x| !rerun(x) && !reads_param(&ps[x]))
}

/// Whether fill pipeline `k` takes `spool_fill`'s first-run form (`first_run`): its deferred facts where no form before it
/// (`whole_first`, the held form, `first_pull`, `first_sort`) takes it.
pub(crate) fn first_ran(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return false };
    if !matches!(crate::rerun::spool::readers::reader_or_copies(gx, k), Ok((_, false))) {
        return false;
    }
    let (o, j) = ps.inners()[reader - ps.len()].0;
    o < ps.len()
        && matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if deferred_child(gx, k, o, inv, rf, vol))
        && !whole_first(gx, k, o, j)
        && !held_child(gx, k, o, j)
        && !first_pull(gx, k, o, j)
        && !first_sort(gx, k, o, j)
        && first_run(gx, k, o, j)
}

/// Whether fill pipeline `k` takes `spool_fill`'s first-pull form (`first_pull`): its deferred facts where neither
/// `whole_first` nor the held form (`held_child`) takes it.
pub(crate) fn first_pulled(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return false };
    if !matches!(crate::rerun::spool::readers::reader_or_copies(gx, k), Ok((_, false))) {
        return false;
    }
    let (o, j) = ps.inners()[reader - ps.len()].0;
    o < ps.len()
        && matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if deferred_child(gx, k, o, inv, rf, vol))
        && !whole_first(gx, k, o, j)
        && !held_child(gx, k, o, j)
        && first_pull(gx, k, o, j)
}

/// Whether fill pipeline `k` takes `spool_fill`'s first-sort form (`first_sort`, D4102): its deferred facts where no form
/// before it (`whole_first`, the held form, `first_pull`) takes it.
pub(crate) fn first_sorted(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return false };
    if !matches!(crate::rerun::spool::readers::reader_or_copies(gx, k), Ok((_, false))) {
        return false;
    }
    let (o, j) = ps.inners()[reader - ps.len()].0;
    o < ps.len()
        && matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if deferred_child(gx, k, o, inv, rf, vol))
        && !whole_first(gx, k, o, j)
        && !held_child(gx, k, o, j)
        && !first_pull(gx, k, o, j)
        && first_sort(gx, k, o, j)
}

/// D1108 row 3: pipeline `p` (its embedded nest inners too) reads an executor parameter in its source or an op; a source or
/// op this reading does not know counts as one.
pub(crate) fn reads_param(p: &gravity_decompose::PipelineNode<'_>) -> bool {
    reads_param_in(p, None, &exec_param)
}

/// `reads_param` where `param` names an expression that reads one; `union`: a settled union's Union op reads one only when
/// its pruning is one `union` names (rowsecurity#343, the emit filling the Material the arm copies read: it reads its
/// feeds' record; `None`: every Union op reads one).
fn reads_param_in(p: &gravity_decompose::PipelineNode<'_>, union: Option<&dyn Fn(&PruneDesc<'_>) -> bool>, param: &dyn Fn(Node<'_>) -> bool) -> bool {
    crate::stack_check(); // D3832
    use gravity_decompose::ops::shared::append::AppendDesc;
    use gravity_decompose::ops::shared::sort::SortForm;
    let any = |l: &types_nodes::list::NodeList<'_>| l.iter().any(param);
    let bound = |b: &gravity_decompose::graph::Bound<'_>| matches!(b, gravity_decompose::graph::Bound::Expr(n) if param(*n));
    let src = match &p.source {
        Source::Range { .. } | Source::Breaker { .. } | Source::Const => false,
        Source::Index { keys, recheck, .. } => keys.iter().any(|x| param(x.value)) || any(recheck),
        // a VALUES list: each row's expressions (`lists` holds a List per row, a node `exec_param`'s walk does not know)
        Source::Values { lists, .. } => lists.iter().any(|l| l.as_list().is_none_or(|r| r.iter().any(param))),
        // D1108: a bitmap heap scan's index keys and recheck (its bitmap is built at the scan's first fetch)
        Source::Bitmap { plan, recheck, .. } => bitmap_reads(plan, param) || any(recheck),
        _ => true,
    };
    // D1108: a LIMIT under the Material reads one only through a computed bound (frame::iface's bounds)
    let limit = |op: &Op<'_>| crate::frame::iface::limit_bounds(op).map(|(count, offset)| bound(count) || bound(offset));
    src || p.ops.iter().any(|op| limit(op).unwrap_or_else(|| match op {
        Op::Filter(n) | Op::Gate(n) => param(*n),
        Op::Project(l) => any(l),
        Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => any(keys) || any(clauses) || any(qual),
        Op::Join(JoinDesc::Build { keys, .. }) => any(keys),
        Op::Join(JoinDesc::Merge { clauses, qual, .. }) => any(clauses) || any(qual),
        // D3692: the params a nest under the Material binds per outer row are its own (PostgreSQL's NestLoop sets them for
        // its inner; the Material's chgParam never holds them, so no rescan of the Material refills over them): its inner
        // reads one only when it reads another; the values bound read one as any expression does
        Op::Join(JoinDesc::Nest { inner, qual, params, .. }) => {
            let values = params.iter().any(|n| n.as_nest_loop_param().is_none_or(|x| param(x.paramval)));
            let outside = |n: Node<'_>| param(n) && exec_param_but(n, params);
            any(qual) || values || reads_param_in(inner, None, &outside)
        }
        // D3692: a nest inner's Memoize reads one through its cache keys; a right or full join's drain of the build rows no
        // probe matched reads the build's table only; a FROM function call through its arguments and target list
        Op::Join(JoinDesc::Memo { keys, .. }) => any(keys),
        Op::Join(JoinDesc::Drain { .. }) => false,
        Op::Srf(d) => d.functions.iter().any(|f| any(f.args)) || any(d.tlist),
        Op::Join(JoinDesc::Spool) | Op::Append(AppendDesc::Arm { .. } | AppendDesc::Feed { .. }) => false,
        Op::Append(AppendDesc::Union { prune, .. }) => union.is_none_or(|f| prune.as_ref().is_some_and(f)),
        // D1108: a Sort under the Material (PostgreSQL's Sort reads its whole input at its first fetch, a rescan of the
        // Material rewinds its store) reads one only through its target list or a computed bound
        Op::Sort(d) => any(d.tlist) || matches!(&d.form, SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } if bound(count) || bound(offset)),
        _ => true,
    }))
}

/// `reads_param_in`'s bitmap plan: an index key of a leaf `param` names.
fn bitmap_reads(plan: &gravity_decompose::ops::shared::scan::BitmapPlan<'_>, param: &dyn Fn(Node<'_>) -> bool) -> bool {
    crate::stack_check();
    use gravity_decompose::ops::shared::scan::BitmapPlan;
    match plan {
        BitmapPlan::Index { keys, .. } => keys.iter().any(|k| param(k.value)),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().any(|p| bitmap_reads(p, param)),
    }
}

/// errorhold (D1108 row 2): a departure that may raise could take the recheck licence here (outside the plan-order run,
/// the statement acting nowhere): `Px::run_ahead`'s licence question without its per-pipeline half.
pub(crate) fn licensable(gx: &Gx<'_, '_>) -> bool {
    use gravity_decompose::effects::{licence, Departure, Effects};
    licence(Effects { acts: gx.graph.acts, ..Effects::RAISES }, Departure::Ahead).is_ok()
}

/// D3373 (lifetime/material.rs `spool_fill`'s last arm): D1107's eager refill of fill pipeline `k`, whose reader's facts
/// say it may raise, runs ahead of PostgreSQL's pulls under the unchecked licence when it calls no VOLATILE function
/// (`waited_fill`), no user code is in it or what feeds it (`effects::fed_user_code`), nothing locks rows, and `licence_unchecked`
/// grants it for the statement (an acting statement is denied, and a fill that hides a call from the side class is asked
/// as user code: `effects::hides_user_code`, as `unchecked_arm` asks); else the statement keeps D1107's and D1108's names.
pub(crate) fn refill_ahead(gx: &Gx<'_, '_>, k: usize, rf: bool, vol: Volatility) -> bool {
    use gravity_decompose::effects::{licence_unchecked, Effects};
    crate::effects::waited_fill(rf, vol)
        && !crate::effects::fed_user_code(gx, k)
        && !gx.graph.pipelines.iter().any(crate::lifetime::material::locks)
        && licence_unchecked(Effects::RAISES, crate::effects::hides_user_code(gx, k)).is_ok()
}

/// errorhold (D1108 row 2): fill pipeline `k` takes `spool_fill`'s eager form by its licensed arm, or (`DECISIONS.md` 4 (D)
/// b) by `unchecked_arm` (facts that may raise, no arm of PostgreSQL's own order taking them), and is not a child of
/// constants only (`quiet`), so its lowering asks the recheck licence and then `Px::ahead_unchecked`.
pub(crate) fn eager_licensed(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, false)) = crate::lifetime::material::spool_fill(gx, k) else { return false };
    let (o, j) = ps.inners()[reader - ps.len()].0;
    let many = crate::rerun::spool::readers::reader_or_copies(gx, k).is_ok_and(|(_, many)| many);
    let arm = |inv, rf, vol| licensed_arm(gx, k, o, inv, rf, vol) || !rf && !deferred_form(gx, k, o, j, many, (inv, rf, vol)) && unchecked_arm(gx, k, o, inv, rf, vol);
    matches!(ps[o].ops.get(j).and_then(reader_facts), Some((inv, rf, vol)) if arm(inv, rf, vol)) && !crate::effects::visible::material::quiet(gx, k)
}

/// G2(e), S1u (S1's pull form over a settled union's arms): pipeline `k` fills the plan Material from a settled union
/// (its source the union's record, its first op the `Union`, no `Feed` into an outer union) and the reader may pull the
/// union as PostgreSQL's Material pulls its Append: arm 0 to its end, then arm 1, each arm opened at its first pull, a
/// row made only when a reader first reaches it (rerun/inner_arms.rs's `Arms` made inside the reader, ops/material/lower.rs).
/// Every feed, one per child in arm order, is a heap range of filters and projections (`Arms`'s arm) and the emit's own
/// ops are filters and projections, none reading an executor parameter (D1107: PostgreSQL refills the Material when one
/// changes) but a fixed, unseen InitPlan's (`init_read`); a pruning is an initial one (decided at the executor start; each
/// arm's skip bit is read at its first pull) or a run-time one over such InitPlans' parameters only (D3297, `init_steps`:
/// decided once per execution, as union_reads.rs `init_pruned`; append/prune.rs `declare` takes it); no rerun set re-runs
/// the emit or an arm (D1107). The feeds' and the emit's own pipelines read nothing. `Ok`: the feeds.
pub(crate) fn union_pull(gx: &Gx<'_, '_>, k: usize) -> Result<Vec<usize>, &'static str> {
    let ps = gx.pipelines();
    let p = ps.get(k).filter(|_| k < ps.len()).ok_or(LOUD_CHILD)?;
    let Source::Breaker { slot } = p.source else { return Err(LOUD_CHILD) };
    let Some((Op::Append(AppendDesc::Union { slot: s, children, prune }), rest)) = p.ops.split_first() else { return Err(LOUD_CHILD) };
    let Some((Op::Join(JoinDesc::Spool), body)) = rest.split_last() else { return Err(LOUD_CHILD) };
    if *s != slot || rest.iter().any(|op| matches!(op, Op::Append(_))) || !matches!(body.last(), Some(Op::Project(_))) {
        return Err(LOUD_CHILD);
    }
    // D3612: a run-time pruning over InitPlans that may raise or act (`pull_late`) is decided at the first pull, which runs
    // them there, so the arms' filters read their fixed parameters after it
    let late = pull_late(gx, k);
    let read = |n: Node<'_>| !exec_param(n) || init_read(gx, n) || late && init_read_by(n, &|id| init_once(gx, id));
    let plain = |ops: &[Op<'_>]| {
        ops.iter().try_for_each(|op| match op {
            Op::Filter(q) if read(*q) => Ok(()),
            Op::Project(l) if l.iter().all(read) => Ok(()),
            Op::Filter(_) | Op::Project(_) => Err(REFILL_CHILD),
            _ => Err(LOUD_CHILD),
        })
    };
    plain(body)?;
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, slot);
    if feeds.is_empty() || feeds.len() != *children as usize {
        return Err(LOUD_CHILD);
    }
    // D3297 (regress partition_prune#424: `ab_a2.b = (select 1)` over ab_a2's partitions under a nest's Material): PG's
    // Append decides a run-time pruning over InitPlans' parameters at its first call, once per execution (no InitPlan has
    // parameters: decompose ops/subplan refuses one; none a rerun set re-runs: `init_fixed`), so it is the initial
    // pruning's form at the reader's first pull, when those InitPlans are unseen and the steps quiet (D2013
    // `quiet_prune` with those parameters bound: effects nestloop.rs `quiet_prune_bound`): which arms the steps let open is PG's
    if prune.as_ref().is_some_and(|d| d.exec && !init_prune(gx, k, d)) && !late {
        return Err(LOUD_CHILD);
    }
    for &f in feeds.iter() {
        let q = &ps[f];
        let Some((Op::Append(AppendDesc::Feed { slot: fs, ordered: false }), arm)) = q.ops.split_last() else { return Err(LOUD_CHILD) };
        if *fs != slot || !matches!(q.source, Source::Range { am: Am::Heap, .. }) || !q.desc.consumes.is_empty() || !matches!(arm.last(), Some(Op::Project(_))) {
            return Err(LOUD_CHILD);
        }
        plain(arm)?;
        crate::rerun::spool::readers::rerun_free(gx, f)?;
    }
    crate::rerun::spool::readers::rerun_free(gx, k)?;
    Ok(feeds)
}

/// D1107's eager refill: fill pipeline `k` is a member of one rerun set, a nest's (plan 0) whose inner embeds the reader's
/// nest (the reader two or more levels below it, join#782). That nest's re-open runs the set before its inner opens
/// (rerun/sets.rs `owner_set`'s D1107 clause), each run empties the record and fills it again (ops/material/lower.rs
/// `Fill`), PostgreSQL's ExecReScanMaterial over a changed parameter, and between two runs every rescan of the reader reads
/// the same rows. IMMUTABLE and raise-free: the fill runs at the re-open even when the reader's nest reads no outer row,
/// where PostgreSQL never runs the child.
pub(crate) fn refilled(gx: &Gx<'_, '_>, k: usize, reader: usize) -> bool {
    let ps = gx.pipelines();
    let mut sets = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(k as u16)));
    let (Some(r), None) = (sets.next(), sets.next()) else { return false };
    if r.plan_id != 0 {
        return sub_refilled(gx, r, reader);
    }
    // the reader's own nest first (its set would re-run per reader re-open: nest/inner.rs refuses that by D1107), then
    // each nest above it up to its graph pipeline
    let (mut q, mut own) = (reader, true);
    while q >= ps.len() {
        let ((c, j), _) = ps.inners()[q - ps.len()];
        if !own && crate::rerun::inner::op_node(&ps[c], j) == r.node_id {
            return true;
        }
        (q, own) = (c, false);
    }
    false
}

/// join#500 (D1108, D1107's refill with a general child): fill pipeline `k` and every pipeline feeding it are members of
/// the one rerun set that holds `k` or kept nest spools, so each run of that set (a re-open over changed parameters)
/// runs the whole child again before the fill, as ExecReScanMaterial's child rescan does (a Material below it with no
/// changed parameter rewinds).
pub(crate) fn one_set(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let mut sets = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(k as u16)));
    let (Some(r), None) = (sets.next(), sets.next()) else { return false };
    let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    // a feeder outside the set is a nest spool's fill that reads no executor parameter: PostgreSQL's rescan of the child
    // rewinds that Material (no changed parameter under it), and the set's run reads the kept spool again
    let kept = |x: usize| gx.spool_kind(x) == KindId::NestSpool && !reads_param(&ps[x]);
    crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src)).into_iter().all(|x| r.pipelines.contains(&(x as u16)) || kept(x))
}

/// D1107 under a nested SubPlan (alter_table / foreign_key's psql trigger list: the parent trigger's SubPlan, a Nested Loop
/// over pg_partition_ancestors(t.tgrelid) with a Materialize over pg_trigger filtered by tgname = t.tgname): rerun set
/// `r` is a correlated SubPlan's and the reader sits in that SubPlan's own embedded inner at any depth (the walk up from
/// the reader meets the SubPlan op whose set is `r`). ExecScanSubPlan marks every parameter changed at each call, so
/// ExecReScanMaterial drops the store and re-runs the child per call; the set's run at each call empties the record and
/// fills it again (ops/material/lower.rs `Fill`) before the SubPlan's inner opens, and the nest inside it rewinds the
/// record per outer row (rerun/sets.rs `members_close` reads the fill as re-run: `refill_member`).
fn sub_refilled(gx: &Gx<'_, '_>, r: &gravity_decompose::graph::Rerun, reader: usize) -> bool {
    let ps = gx.pipelines();
    let mut q = reader;
    while q >= ps.len() {
        let ((c, j), _) = ps.inners()[q - ps.len()];
        if matches!(ps[c].ops.get(j), Some(Op::SubPlan(_))) && crate::rerun::sets::sub_set(gx, q).is_some_and(|s| core::ptr::eq(s, r)) {
            return true;
        }
        q = c;
    }
    // or a member of `r` itself embeds it (a Sort's or an aggregate's input under the SubPlan's plan): the run fills the
    // record before that member runs (it reads the spool's slot, so the program orders it after the fill)
    r.pipelines.contains(&(q as u16))
}

/// D1107's eager refill as a rerun set member (rerun/sets.rs `members_close`): fill pipeline `m` is a member of a nested
/// SubPlan's set and takes the eager form (`spool_fill`: a member reaches it only through `refilled`), so each run of the
/// set empties its record and fills it again, and its one reader, inside the SubPlan's own inner, reads it from 0. So is a
/// feed of a settled union the set refills whole (D1483 S2q, rerun/union_arms.rs `refed`), its emit a member too.
pub(crate) fn refill_member(gx: &Gx<'_, '_>, m: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.plan_id != 0 && r.pipelines.contains(&(m as u16)))
        && (matches!(crate::lifetime::material::spool_fill(gx, m), Ok((_, false))) || crate::rerun::union_arms::refed(gx, m))
}

/// The FACTS-1b facts the eager form reads off the op that embeds the reader, as (F1 invariant, F5 raise-free, the most
/// volatile call): a nest's over its inner, a nested SubPlan's over its plan, test and arguments (material item 6, D1109's
/// SubPlan link: a superset of the inner's, so no weaker). `None`: another op (no reader of the spool's pull or eager form).
pub(crate) fn reader_facts(op: &Op<'_>) -> Option<(bool, bool, Volatility)> {
    match op {
        Op::Join(JoinDesc::Nest { facts, .. }) => Some((facts.invariant, facts.raise_free, facts.volatility)),
        Op::SubPlan(d) if matches!(d.form, SubForm::Nested { .. }) => Some((d.invariant, d.raise_free, d.volatility)),
        _ => None,
    }
}

/// D3297: executor parameter `id` is set by InitPlans only, none of which acts or may raise (pgrc.rs `init_unseen`,
/// D2875: F5, a second row's 21000 included) and none of which a rerun set runs again (its plan reads no nest's or
/// SubPlan's parameter, as an InitPlan inside a correlated SubPlan over the outer query's column does: PG re-runs that
/// one per changed parameter, ExecReScanSetParamPlan). Such a parameter is fixed for the statement, so PG's Material
/// never refills over it (ExecReScanMaterial's chgParam), and PG running its InitPlan at the first read, or never, shows
/// nothing.
pub(crate) fn init_fixed(gx: &Gx<'_, '_>, id: i32) -> bool {
    let rerun = |q: u16| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&q));
    crate::effects::visible::pgrc::init_unseen(gx, id) && gx.graph.init_plans.iter().filter(|i| i.params.iter().any(|p| p == id)).all(|i| !rerun(i.pipeline))
}

/// D3297: every executor parameter `n` reads is `init_fixed`, and the walk knows every node.
pub(crate) fn init_read(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    init_read_by(n, &|id| init_fixed(gx, id))
}

/// `init_read` with `fixed` the parameter test.
fn init_read_by(n: Node<'_>, fixed: &dyn Fn(i32) -> bool) -> bool {
    let mut ok = true;
    let known = gravity_decompose::expr::walk(n, &mut |x| {
        if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
            ok = fixed(p.paramid);
        }
        ok
    });
    ok && known
}

/// D3612: executor parameter `id` is set by InitPlans only, none of which a rerun set runs again (`init_fixed` without
/// `init_unseen`: they may raise or act). PG runs such an InitPlan once, at the first read of its parameter.
pub(crate) fn init_once(gx: &Gx<'_, '_>, id: i32) -> bool {
    let rerun = |q: u16| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&q));
    let mut sets = gx.graph.init_plans.iter().filter(|i| i.params.iter().any(|p| p == id)).peekable();
    sets.peek().is_some() && sets.all(|i| !rerun(i.pipeline))
}

/// D3714: `init_once` where no InitPlan setting `id` acts (a VOLATILE call, or a lock or write in its pipelines): they
/// may raise only.
pub(crate) fn init_late(gx: &Gx<'_, '_>, id: i32) -> bool {
    let ps = gx.pipelines();
    let acts = |q: u16| gx.with_producers([usize::from(q)]).into_iter().any(|x| ps[x].ops.iter().any(|op| matches!(op, Op::Lock(_) | Op::Write(_))));
    init_once(gx, id) && gx.graph.init_plans.iter().filter(|i| i.params.iter().any(|p| p == id)).all(|i| !gravity_decompose::effects::Effects::of_init_plan(i).acts && !acts(i.pipeline))
}

/// D3714: the run-time pruning `d` of the settled union pipeline `e` emits reads `init_late` parameters, exactly one
/// of them not `init_fixed` (its InitPlans may raise), and that one is the only parameter a root level's exec steps
/// read, which PostgreSQL's ExecFindMatchingSubPlans runs at its every decision (a sub-level only for a surviving
/// child): so it runs those InitPlans at the Append's first call, never over an empty outer, as exec's src.prune
/// decides it at the first arm's start reading every Init (ops/append/prune_exec.rs `LatePrune`). The steps are quiet
/// with those parameters bound. `Some`: that parameter.
pub(crate) fn late_param(gx: &Gx<'_, '_>, e: usize, d: &PruneDesc<'_>) -> Option<i32> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps.get(e).filter(|_| e < ps.len())?.source else { return None };
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    let mut late = None;
    let over = d.exec_params_all(|id| init_fixed(gx, id) || init_late(gx, id) && *late.get_or_insert(id) == id);
    let id = late.filter(|_| d.exec && over && crate::rerun::union_reads::init_pruned(gx, d, &feeds))?;
    let alone = |r: &types_nodes::plannodes::PartitionedRelPruneInfo<'_>| r.execparamids.next_member(-1) == id && r.execparamids.next_member(id) < 0;
    let root = d.info.prune_infos.iter().filter_map(|l| l.as_list()).any(|l| l.iter().next().and_then(|r| r.as_partitioned_rel_prune_info()).is_some_and(alone));
    (root && crate::effects::visible::nestloop::quiet_prune_bound(gx, d, &|x| init_late(gx, x))).then_some(id)
}

/// D3714: executor parameter `id` is `late_param` of a settled union's pruning that a deferred fill runs whole
/// (arm_fill.rs `whole_union`): that fill runs before any row reaches a nest reading it, so its InitPlans ran before
/// any write or EvalPlanQual of the statement, which copies their values down (EvalPlanQualStart) and runs none again.
pub(crate) fn init_at_fill(gx: &Gx<'_, '_>, id: i32) -> bool {
    let ps = gx.pipelines();
    (0..ps.len()).any(|e| matches!(ps[e].ops.first(), Some(Op::Append(AppendDesc::Union { prune: Some(d), .. })) if late_param(gx, e, d) == Some(id)) && crate::rerun::spool::arm_fill::whole_union(gx, e))
}

/// D3612: the settled union pipeline `e` emits is pruned at run time over InitPlans' parameters that are `init_once` but
/// not all `init_fixed` (an InitPlan may raise or act), its steps quiet with them bound. PostgreSQL decides it at the
/// Append's first call, running those InitPlans there, never over an empty outer; the pulled form decides it at the
/// Material's first pull (rerun/inner_arms.rs `pulled`'s nest.prune object, exec reading the InitPlans' cells).
pub(crate) fn pull_late(gx: &Gx<'_, '_>, e: usize) -> bool {
    let ps = gx.pipelines();
    let Some(Op::Append(AppendDesc::Union { prune: Some(d), children, .. })) = ps.get(e).filter(|_| e < ps.len()).and_then(|p| p.ops.first()) else { return false };
    let Source::Breaker { slot } = ps[e].source else { return false };
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    let ids = || d.info.prune_infos.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter_map(|r| r.as_partitioned_rel_prune_info()).flat_map(|r| {
        std::iter::successors(Some(r.execparamids.next_member(-1)).filter(|&x| x >= 0), move |&x| Some(r.execparamids.next_member(x)).filter(|&y| y >= 0))
    });
    d.exec
        && *children <= 63
        && d.index != gravity_decompose::facts::hoist::HOISTED
        && !init_prune(gx, e, d)
        && crate::rerun::union_reads::init_pruned(gx, d, &feeds)
        && ids().all(|id| init_once(gx, id))
        && crate::effects::visible::nestloop::quiet_prune_bound(gx, d, &|id| init_once(gx, id))
}

/// D3297: the run-time pruning `d` of the settled union pipeline `e` emits is decided once per execution: over fixed,
/// unseen InitPlans' parameters only (union_reads.rs `init_pruned` over its feeds, `init_steps`), its steps quiet with
/// those parameters bound (effects nestloop.rs `quiet_prune_bound`).
pub(crate) fn init_prune(gx: &Gx<'_, '_>, e: usize, d: &PruneDesc<'_>) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[e].source else { return false };
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    crate::rerun::union_reads::init_pruned(gx, d, &feeds) && init_steps(gx, d) && crate::effects::visible::nestloop::quiet_prune_bound(gx, d, &|id| init_fixed(gx, id))
}

/// D3297: every executor parameter a run-time pruning's steps read (each prune info's `execparamids`) is `init_fixed`
/// (union_reads.rs `init_pruned` has them InitPlans' already).
fn init_steps(gx: &Gx<'_, '_>, d: &gravity_decompose::ops::shared::append::PruneDesc<'_>) -> bool {
    d.info.prune_infos.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).all(|r| {
        let Some(r) = r.as_partitioned_rel_prune_info() else { return false };
        let mut x = r.execparamids.next_member(-1);
        while x >= 0 {
            if !init_fixed(gx, x) {
                return false;
            }
            x = r.execparamids.next_member(x);
        }
        true
    })
}

/// Whether `n` reads an executor parameter, or holds a node the walk does not know (read as one; effects/visible/material.rs too).
pub(crate) fn exec_param(n: Node<'_>) -> bool {
    let mut hit = false;
    let known = gravity_decompose::expr::walk(n, &mut |x| {
        hit |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC);
        !hit
    });
    hit || !known
}

/// D3692: `exec_param` but for the PARAM_EXEC params `own` names (those a nest under the Material binds for its inner).
fn exec_param_but(n: Node<'_>, own: &types_nodes::list::NodeList<'_>) -> bool {
    let mut hit = false;
    let known = gravity_decompose::expr::walk(n, &mut |x| {
        hit |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && !own.iter().any(|m| m.as_nest_loop_param().is_some_and(|o| o.paramno == p.paramid)));
        !hit
    });
    hit || !known
}
