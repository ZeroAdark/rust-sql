//! D1906: a rerun set's member probe over a hash table kept across the set's calls (its build in no
//! rerun set) whose build side calls a user or VOLATILE function, visibly (the side class's UserFn, probe.rs `opaque`) or
//! through code the side class does not read (`hidden`: a value of a user type, whose support procs are user code, or a
//! row-reading xml builtin: effects.rs `user_typed`). On the outer-first path PG 18.6 builds at the first call whose outer
//! has a row (HJ_BUILD_HASHTABLE) and reuses a one-batch table at every later call (ExecReScanHashJoin: nbatch 1, no
//! changed parameter); a multi-batch table is destroyed and built again at the next call. Served over a lazy build (D824:
//! it runs when the first row reaches the probe, PG's build call, so the side runs on no call PG never builds at and
//! raises nothing PG would not), kept across the calls and grown at run time (D254) whatever its size.
//! D2713 / D2712 (a side's function is called at least once, and any number of times more; the
//! "never fewer calls" half dropped, keep or rebuild chosen on speed): PG's rebuild of a multi-batch table only repeats
//! calls, so the table is kept at any size: no run-time cap past which the probe left with UNSUPPORTED_SHAPE (XX000 where
//! PG answers), no refusal of an estimate past a one-batch bound.
//! D2615 / D4103: in a recursive union's term (a generation run by the driver, ops/setop/recursive.rs) the probe takes
//! the road as anywhere else. A lazy group that fanned in (a set operation's emit over its two fills, a union's arms into
//! one Sort or group) or held a hashed aggregate's fill refused by D1906's name there (D2615's fail-closed `fans_in` and
//! `hashed_agg`), because its run did not build (the probe found no rows) and then ended with UNSUPPORTED_SCRATCH. Both
//! were one fault, D4189 (exec ops/ctescan/proto.rs `members`): the term's CTE pull ran the group ahead of the probe and
//! the probe's wait ran the build again over its feeders' freed tables. With the group left to the wait it runs once, at
//! the first row that reaches the probe, and the table is kept across generations as PostgreSQL 18.6 keeps a one-batch
//! table (a multi-batch one PG rebuilds at a later generation, calling the side's functions again: the count-free
//! departure, D2713).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};

use crate::effects::user_typed;
use crate::graph::Gx;
use crate::ops::hashjoin::build_class::{feed, side_by, Side};

/// The road serves probe `j` of `q` over `b`: `kept_road` (the lifetime home's), its build lazy (`lazy`, made before), and an outer side that
/// is quiet or takes D1904's later-call words (its source and feeders quiet: they run at every call).
pub(crate) fn kept(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize, lazy: bool) -> bool {
    lazy && crate::lifetime::hashjoin::kept_road(gx, q, j, b) && (side_by(gx, q, j, true) == Side::Safe || side_by(gx, q, 0, false) == Side::Safe)
}

/// D1904's later-call road (probe_calls.rs `Calls`): its own test, or this file's kept build under an outer side that is not
/// quiet (each later call scans the whole outer over the kept table, an empty one too, as PG's rescan does), or a nest
/// inner's kept probe whose outer keys may raise (D3378 C3: probe_calls.rs `nest_rescans`).
pub(crate) fn rescans(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize, lazy: bool) -> bool {
    crate::ops::hashjoin::probe_calls::rescans(gx, q, j, b) || kept(gx, q, j, b, lazy) && side_by(gx, q, j, true) != Side::Safe || crate::ops::hashjoin::probe_calls::nest_rescans(gx, q, j, b)
}

/// Build `b`'s side (`b` and its feeders) holds code the side class does not read: a value of a user type or a
/// row-reading xml builtin (`user_typed`), or a builtin running a named type's input (`input_fn`), in an op, its source's
/// expressions, or a feeder group's keys, aggregates and FILTERs (a group by a user range compares by its subtype's user
/// comparison).
pub(crate) fn hidden(gx: &Gx<'_, '_>, b: usize) -> bool {
    let p = &gx.pipelines()[b];
    let all = feed(gx, b, p.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = p.source { Some(slot) } else { None }));
    let h = |n: types_nodes::Node<'_>| user_typed(gx, n) || input_fn(n);
    let op = |o: &Op<'_>| match o {
        Op::Filter(n) | Op::Gate(n) => h(*n),
        Op::Project(l) => l.iter().any(h),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().any(h),
        Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => clauses.iter().chain(keys.iter()).chain(qual.iter()).any(h),
        Op::Srf(d) => d.functions.iter().any(|f| h(f.node)),
        Op::SubPlan(d) => d.test.into_iter().chain(d.args.iter()).any(h),
        _ => false,
    };
    let src = |s: &Source<'_>| match s {
        Source::Index { keys, recheck, .. } => keys.iter().map(|k| k.value).chain(recheck.iter()).any(h),
        Source::Bitmap { recheck, .. } => recheck.iter().any(h),
        Source::Values { lists, .. } => lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(h),
        Source::Tid { quals, .. } => quals.iter().any(h),
        _ => false,
    };
    all.into_iter().chain(std::iter::once(b)).any(|x| {
        let p = &gx.pipelines()[x];
        let group = gx.group_at(x).is_some_and(|g| g.keys.iter().any(|k| h(k.expr.node())) || g.aggs.iter().any(|a| h(a.node) || a.filter.is_some_and(h)));
        p.ops.iter().any(op) || src(&p.source) || group
    })
}

/// pg_input_is_valid / pg_input_error_info (`types_core::quiet::INPUT_VALIDATORS`): they run a named type's input function and a
/// domain's CHECK, user code the side class does not read.
fn input_fn(n: types_nodes::Node<'_>) -> bool {
    let mut any = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        any |= x.as_func_expr().is_some_and(|f| types_core::quiet::input_validator(f.funcid));
        !any
    });
    any
}
