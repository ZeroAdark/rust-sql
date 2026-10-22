//! material's lifetime decision, which the family asks here: how a plan Material's rows are kept across its nest's
//! rescans. PostgreSQL's Material is lazy (nodeMaterial.c ExecMaterial reads its child one row per call) and its rescan
//! rewinds the store, discarding it and re-reading the child only when a parameter the child reads changed or no rewind
//! was asked (ExecReScanMaterial). `spool_fill` picks the pull form (that store, pulled as PostgreSQL pulls it), the eager
//! form (filled up front once, where nothing PostgreSQL shows can tell) or the refusal that names why neither keeps the
//! rows as PostgreSQL does. The forms' shape tests and refusal names stay in rerun/spool/form.rs.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::catalog::Am;
use gravity_decompose::effects::Licence;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};

use crate::face::kinds::KindId;
use crate::graph::Gx;
use crate::rerun::spool::form::{
    deferred_child, exec_param, first_pull, first_sort, held_child, licensed_arm, one_set, reader_facts, refill_ahead, refilled, result_child, unchecked_arm,
    union_pull, whole_first, whole_first_arms,
    LOUD_CHILD, MANY_READERS, PGRC_CHILD, REFILL_CHILD, WALK_BOUND_AT_BIND,
};

/// Pipeline `k`'s plan Material and its one nest inner reader: `(inner, true)`, the pull form, when the child is one scan
/// (heap, index, index-only) of filters and projections that reads no executor parameter; `(inner, false)`, the eager form,
/// when the nest's facts (FACTS-1b) say every rescan of the Material returns the same rows and nothing under it can raise
/// (F1 invariant, F5 raise-free, or a child of constants only with no VOLATILE call: effects/visible/material.rs; its first run may be kept, and an
/// early fill or an empty outer changes nothing PostgreSQL shows): the child's pipelines run first and `k`'s end appends
/// each row. Else `Err` names why (D1107 / D1108 / D1109;
/// D1107 too when a rerun set re-runs `k`: spool_readers.rs `rerun_free`; two levels down, D1630 for an inner of the graph
/// whose walk binds at bind). D1637's eager sweep (the child runs before the nest first reads the Material, an empty outer
/// included, where PostgreSQL runs it only at its first read) is the eager arm's own F5 condition: a STABLE call fills up
/// front only when F5's per-call predicate takes it (decompose effects found.rs `call_raise_free`: a quiet builtin, or a
/// LANGUAGE internal function over one), so it raises on no input, and STABLE code does not act.
/// D2780: whether pipeline `k`'s pull form reads its child on to the end of the window it reached at each pull, appending
/// every row that passes (rerun/spool/child.rs `Child::rest`), where PostgreSQL's Material reads one row a call. Taken when
/// what the child does before its reader asks for it shows nothing: the reader's facts are calm (F1 invariant, F5
/// raise-free, no VOLATILE call: effects/visible/material.rs `calm_fill`, nest.selfspool's own facts), the statement acts
/// on nothing, and no LockRows positions a mark at the child's site (lockrows J182: the recheck pulls the marked row at
/// that site). A reader that stops early reads at most the rest of that window ahead.
pub(crate) fn pull_reads_on(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Ok((reader, true)) = spool_fill(gx, k) else { return false };
    let Some((o, j)) = reader.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).map(|x| x.0)) else { return false };
    let calm = o < ps.len() && ps[o].ops.get(j).and_then(reader_facts).is_some_and(|(inv, rf, vol)| crate::effects::visible::material::calm_fill(inv, rf, vol));
    calm && !gx.graph.acts && !gx.graph.pipelines.iter().any(locks)
}

/// Whether pipeline `p` (or an inner it embeds) holds a LockRows.
pub(crate) fn locks(p: &gravity_decompose::PipelineNode<'_>) -> bool {
    crate::stack_check(); // D3832
    p.ops.iter().any(|o| match o {
        Op::Lock(_) => true,
        Op::Join(JoinDesc::Nest { inner, .. }) => locks(inner),
        Op::SubPlan(d) => matches!(&d.form, gravity_decompose::ops::shared::subplan::SubForm::Nested { inner } if locks(inner)),
        _ => false,
    })
}

pub(crate) fn spool_fill(gx: &Gx<'_, '_>, k: usize) -> Result<(usize, bool), &'static str> {
    fill_form(gx, k, true).map(|(reader, pull, _)| (reader, pull))
}

/// Construct C2: the licence of pipeline `k`'s eager fill when `spool_fill` took it by an arm whose own test proves it
/// quiet (calm, constants only, D1107's raise-free refill): `effects::unobservable`. `None` for the pull form and for
/// every other eager arm (ops/material/lower.rs `make_end` holds their licences in `Fill.ahead`, or the fill runs at
/// the nest's first outer row, PostgreSQL's first fetch).
pub(crate) fn quiet_fill_licence(gx: &Gx<'_, '_>, k: usize) -> Option<Licence> {
    fill_form(gx, k, true).ok().and_then(|f| f.2)
}

/// D3373: fill pipeline `k` takes `spool_fill`'s eager form by its last arm alone, D1107's eager refill of a child that
/// may raise (form.rs `refill_ahead`), so its lowering asks the unchecked licence (ops/material/lower.rs).
pub(crate) fn refilled_ahead(gx: &Gx<'_, '_>, k: usize) -> bool {
    fill_form(gx, k, false).is_err() && fill_form(gx, k, true).is_ok()
}

/// `spool_fill`, with the licence of a quiet eager arm's fill (`quiet_fill_licence`); `ahead`: its last arm
/// (`refilled_ahead`) is open.
fn fill_form(gx: &Gx<'_, '_>, k: usize, ahead: bool) -> Result<(usize, bool, Option<Licence>), &'static str> {
    let ps = gx.pipelines();
    let Some(p) = ps.get(k).filter(|_| k < ps.len()) else { return Err("") };
    if !p.desc.breaker().is_some_and(|b| b.kind == BreakerKind::Materialize) || gx.spool_kind(k) != KindId::NestSpool {
        return Err("");
    }
    // D1919 (R1): the one pipeline that reads the life, an owner being any that reads it only through an inner it embeds;
    // or (rowsecurity#343, `many`) the nest copies of a streamed write's arms, eager or deferred forms only (a pull keeps
    // its position per reader)
    let (reader, many) = crate::rerun::spool::readers::reader_or_copies(gx, k)?;
    let Some((Op::Join(JoinDesc::Spool), body)) = p.ops.split_last() else { return Err(LOUD_CHILD) };
    if !matches!(body.last(), Some(Op::Project(_))) {
        return Err(LOUD_CHILD);
    }
    // the child's answer as a heap child's: D1107 (it reads an executor parameter) or D1108 (not one scan of filters and
    // projections)
    let heap_like = || {
        match &p.source {
            Source::Range { .. } => {}
            Source::Index { keys, recheck, .. } if keys.iter().any(|x| exec_param(x.value)) || recheck.iter().any(exec_param) => return Err(REFILL_CHILD),
            Source::Index { .. } => {}
            _ => return Err(LOUD_CHILD),
        }
        for op in body {
            match op {
                Op::Filter(q) if !exec_param(*q) => {}
                Op::Project(l) if !l.iter().any(exec_param) => {}
                Op::Filter(_) | Op::Project(_) => return Err(REFILL_CHILD),
                _ => return Err(LOUD_CHILD),
            }
        }
        Ok(())
    };
    // D1484 names a pgrcolumnar source where the pull form would read it; D3176: a child that is not one scan of
    // filters and projections takes no pull form on any source, so it answers D1108 as a heap child does and the eager
    // arms (which run it in the frame's window loop) read it
    let pgrc = matches!(&p.source, Source::Range { am, .. } | Source::Index { am, .. } if *am != Am::Heap);
    let pull = || match heap_like() {
        Err(why) if why == LOUD_CHILD => Err(LOUD_CHILD),
        _ if pgrc => Err(PGRC_CHILD),
        r => r,
    };
    let why = match pull() {
        Ok(()) if !many => return crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, true, None)),
        Ok(()) => MANY_READERS,
        Err(why) => why,
    };
    let (o, j) = ps.inners()[reader - ps.len()].0;
    // D3852: a pgrcolumnar child of the pull form's shape (one scan of filters and projections reading no executor
    // parameter), its one reader's pull form refused by D1484, takes the arms below of a child no pull form takes (D1108's
    // rows 2 and 3, the unchecked arm), as D3248's pgrcolumnar child of another shape does; D1107's refill arms read the
    // name it was refused by (`named`), and none taking it, D1484 names it
    let named = why;
    let why = if why == PGRC_CHILD && !many && heap_like().is_ok() { LOUD_CHILD } else { why };
    // G2(e), S1u: a settled union with an arm that is not quiet refuses D1486 in the eager form (it would run every arm
    // ahead of PostgreSQL's pulls); its one-level reader pulls it arm after arm instead
    let loud = |f: &[usize]| !f.iter().all(|&x| crate::rerun::union_arms::quiet_arm(gx, &ps[x]));
    if !many && o < ps.len() && ps[o].ops.get(j).and_then(reader_facts).is_some() && union_pull(gx, k).is_ok_and(|f| loud(&f[..])) {
        return Ok((reader, true, None));
    }
    // construct C2: the eager arms whose own test proves the fill quiet (raise-free, no VOLATILE call: calm, constants
    // only, D1107's raise-free refill) hold the licence of code that shows nothing (`effects::unobservable`)
    let quiet = || gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE);
    match ps[o].ops.get(j).and_then(reader_facts) {
        // D1630's class where B-R1 serves
        Some((inv, rf, vol)) if o >= ps.len() && crate::effects::visible::material::calm_fill(inv, rf, vol) && crate::rerun::spool::readers::walk_bound(gx) => Err(WALK_BOUND_AT_BIND),
        // the eager form: F5 raise-free (a STABLE call too, as F5's per-call predicate takes it: D1637's eager sweep)
        Some((inv, rf, vol)) if crate::effects::visible::material::calm_fill(inv, rf, vol) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, quiet())),
        // D1108 row 1: a child of constants only (the effects home's material `quiet`) is invariant and raise-free whatever F1 / F5 say
        Some((_, _, vol)) if crate::effects::visible::material::quiet_fill(gx, k, vol) => match o >= ps.len() && crate::rerun::spool::readers::walk_bound(gx) {
            true => Err(WALK_BOUND_AT_BIND),
            false => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, quiet())),
        },
        // D1107's eager refill: a child of one scan, filters and projections that reads a nest's parameters only (the
        // pull form's shape bar its parameters), raise-free with no VOLATILE call (the effects home's `refill_quiet`), re-run
        // by that nest's set alone; or (join#500) a general child whose every pipeline that set re-runs (`one_set`)
        // D3614: raise-free also when the child is a settled union whose arms read only bound words and unseen InitPlans'
        // outputs (nestloop.rs `refill_union_quiet`), which the reader's F5 counts as loud
        Some((_, rf, vol)) if !many && (named == REFILL_CHILD || named == LOUD_CHILD && one_set(gx, k)) && crate::effects::refill_quiet(rf || crate::effects::visible::nestloop::refill_union_quiet(gx, k), vol) && refilled(gx, k, reader) => match o >= ps.len() && crate::rerun::spool::readers::walk_bound(gx) {
            true => Err(WALK_BOUND_AT_BIND),
            false => Ok((reader, false, quiet())),
        },
        // G2(e), S1u: a settled union of quiet arms whose eager form refuses, pulled arm after arm by its one-level reader
        Some(_) if !many && o < ps.len() && union_pull(gx, k).is_ok() => Ok((reader, true, None)),
        // errorhold (D1108 row 2): an invariant IMMUTABLE fill that may raise, filled up front in pipeline order (lazy.rs
        // keeps it out of D2157's lazy fill) under errorhold's recheck licence (ops/material/lower.rs `Px::run_ahead`): its
        // error before any row left re-runs the statement in plan order, where the licence is denied and D1108 answers.
        // Its reader is a one-level nest, or a nest two or more levels down when no inner of the graph walks by a word
        // bound once at bind (D1630's guard, as the raise-free eager arm has: `walk_bound`; else D1108 stays the name)
        Some((inv, rf, vol)) if why == LOUD_CHILD && licensed_arm(gx, k, o, inv, rf, vol) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D1108 row 3: a child that may raise or call VOLATILE code, run whole at the nest's first outer row (`deferred`)
        // (D3373: errorhold's licensed facts too where its arm above cannot take them, `deferred_child`)
        Some((inv, rf, vol)) if why == LOUD_CHILD && deferred_child(gx, k, o, inv, rf, vol) && (whole_first(gx, k, o, j) || many && whole_first_arms(gx, k, o, j)) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D3718 (LOCK-FILL): such a child that takes its own row locks (form.rs `lock_fill`); D4204: pulled one row per
        // fetch, PostgreSQL's ExecMaterial (form.rs `lock_fill_pulls`), else run whole at the nest's first outer row
        Some((inv, rf, vol)) if why == LOUD_CHILD && !many && deferred_child(gx, k, o, inv, rf, vol) && crate::rerun::spool::form::lock_fill(gx, k, o, j) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, crate::rerun::spool::form::lock_fill_pulls(gx, k), None)),
        // D1108 (rxshape1, op 14 "held at len"): such a child whose rows come in PostgreSQL's order and whose only show is its
        // error (form.rs `held_child`), read by a nest that may stop before its end: run whole at the nest's first outer
        // row, its error held at the record it stopped at and raised when the reader reaches that record (form.rs `held`)
        Some((inv, rf, vol)) if why == LOUD_CHILD && !many && deferred_child(gx, k, o, inv, rf, vol) && held_child(gx, k, o, j) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D1108 (rxshape1, first pull): such a child whose every raise and call PostgreSQL makes at the Material's first
        // fetch (a hashed or plain Agg under quiet finals and a quiet target list, form.rs `first_pull`), read by a nest that
        // may stop anywhere: run whole at the nest's first outer row, PostgreSQL's first fetch
        Some((inv, rf, vol)) if why == LOUD_CHILD && !many && deferred_child(gx, k, o, inv, rf, vol) && first_pull(gx, k, o, j) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D4102 (first sort): such a child under a Sort (or a top-N Sort) that PostgreSQL reads whole at the Material's
        // first fetch, past it a quiet LIMIT and quiet filters and projections (form.rs `first_sort`), read by a nest that
        // may stop anywhere, in no rerun set or in a recursive union's generation set alone: run whole at the nest's first
        // outer row, PostgreSQL's first fetch, once for every generation
        Some((inv, rf, vol)) if why == LOUD_CHILD && !many && deferred_child(gx, k, o, inv, rf, vol) && first_sort(gx, k, o, j) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D3803 (first run): such a child whose every call and raise PostgreSQL makes at the Material's first fetch (a FROM
        // function over the Result, the hash builds it probes) and whose per-row work is quiet (form.rs `first_run`), read by
        // a nest that may stop anywhere: run whole at the nest's first outer row, PostgreSQL's first fetch
        Some((inv, rf, vol)) if why == LOUD_CHILD && !many && deferred_child(gx, k, o, inv, rf, vol) && crate::rerun::spool::form::first_run(gx, k, o, j) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D3498: a Result's one row that no eager form takes (a VOLATILE call, a raise, an acting statement), pulled: the
        // reader makes it at its first read, PostgreSQL's first fetch, and reads the record at every later one; a write's
        // recheck reruns the fill over a fresh record (rerun/epq/marks.rs `sub_producer`), as EvalPlanQual's fresh Material
        // makes the row again
        Some(_) if why == LOUD_CHILD && !many && result_child(p, body) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, true, None)),
        // `DECISIONS.md` 4 (D) b: such a child, invariant, not VOLATILE, that may raise, that no arm above takes (the
        // plan-order re-run, where errorhold's licence is denied; or no form in PostgreSQL's own order applies): filled up
        // front unchecked where `licence_unchecked` grants it (form.rs `unchecked_arm`; ops/material/lower.rs asks
        // `Px::ahead_unchecked`, which tags it), else D1108
        Some((inv, rf, vol)) if why == LOUD_CHILD && unchecked_arm(gx, k, o, inv, rf, vol) => crate::rerun::spool::readers::rerun_free(gx, k).map(|()| (reader, false, None)),
        // D3373 (D1107's eager refill ahead): such a child that may raise, with no VOLATILE call and no user code in it
        // or what feeds it, in a statement that acts on nothing (form.rs `refill_ahead`), re-run by that set alone (as
        // above): each run refills it whole where PostgreSQL's Material reads its child only as far as the nest pulls,
        // so the rows are the same and an error from rows PostgreSQL never reads is the unchecked licence's tagged one
        // (`DECISIONS.md` 4 (D) b; ops/material/lower.rs asks `Px::ahead_unchecked` by `refilled_ahead`)
        Some((_, rf, vol)) if ahead && !many && (named == REFILL_CHILD || named == LOUD_CHILD && one_set(gx, k)) && refill_ahead(gx, k, rf, vol) && refilled(gx, k, reader) => match o >= ps.len() && crate::rerun::spool::readers::walk_bound(gx) {
            true => Err(WALK_BOUND_AT_BIND),
            false => Ok((reader, false, None)),
        },
        _ => Err(named),
    }
}
