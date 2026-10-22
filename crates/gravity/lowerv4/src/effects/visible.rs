//! The effects home's visible half (user 10-03; FILE-MAP lowerv4/src/effects/visible.rs, home semantics): may the user
//! see what a strategy changes when it reorders, skips or batches a pipeline's rows? Each question is one plain function
//! returning the first reason it finds, and every reason is a `Why` of this file, so a strategy asks here instead of
//! keeping a private predicate (lint rule RV: no function outside the effects home decides volatility, raise-freedom or
//! row-order visibility; the sites not yet moved are lint/EFFECTS-DEBT rows with their owner lane).
//! - `order_visible`: the order in which pipeline `k`'s source produces rows (the heap walk's reversal, Decision 11).
//! - `calls_visible`: the order and count of pipeline `k`'s VOLATILE calls (D831); `rewind_visible`: the count of the
//!   calls a rewind of the statement would make again where PostgreSQL's rescan keeps their results (D2651).
//! - `error_visible`: which row raises first, when an early-exit consumer may stop before a later row's error (D2593);
//!   `fold_visible`: whether a fold's error depends on the row order beyond what errorhold's recheck repairs (D2597).
//! Planning facts come from decompose (its effects facts on the ops, `crate::graph`'s aggregate classes), never copied.
/// The merge families' moved decisions (EFF-MERGE-APPEND: MergeAppend's raise-free arms, merge.join's quiet inner and outer).
pub(crate) mod merge;
/// join.drain's spooled form's effect readings (spool_admit.rs's quiet, acts, order-free and raise-free tests).
pub(crate) mod drain;
/// lowerv4 ops/material's effects tests (a Material fill run ahead, a nest's Materialize inner): its family's sibling file.
pub(crate) mod material;
/// The cover (D2212): a pipeline run ahead whose raises PG's statement makes too (merge.join's FULL inner, a hash join's
/// outer side run before a build PG makes first).
pub(crate) mod cover;

use gravity_decompose::effects::Volatility;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::Op;
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use mcx::{CollectIn, GrowIn, PgVec};

/// The result family's decision (a pipeline run whole shows nothing: the gate's feeder masks).
pub(crate) mod result;

use gravity_expr::Catalog;

pub(crate) mod expr;

/// group's raise / act decisions (a presorted DISTINCT's riders, a host-sorted aggregate's transitions and final).
pub(crate) mod group;

/// exec admission's effect tests: whether a write's output types reach user code.
pub mod admit;
/// The ctescan family's moved decision (a CTE plan a reader pulls WINDOW rows at a time).
pub(crate) mod ctescan;
/// Construct C2's same-row rule: the one decider of a run ahead at PostgreSQL's own point.
pub(crate) mod same_row;
/// The dict family's moved decision (a code cell's calls).
pub(crate) mod dict;

use crate::effects::fx::PlanFx;
use crate::graph::Gx;
/// lowerv4 ops/memoize's effects tests (a purge's VOLATILE calls): its family's sibling file.
pub(crate) mod memoize;

/// The srf family's decisions (ProjectSet's open error order, D1657's VOLATILE test).
pub(crate) mod srf;
pub(crate) mod hashjoin;
pub(crate) mod nestloop;
/// The lockrows family's REFERENCE rechecks (a refetched row's calls, a build part's rerun).
pub(crate) mod lockrows;
/// The pgrc source's lead-qual quiet reading (whether its verdicts may skip rows for a qual).
pub(crate) mod pgrc;

/// agg's and aggtrans's decisions (EFF-AGG / EFF-AGGTRANS: the walk skip's raise-freedom, the hosted path's sort and
/// order freedom, finals_cse's quiet test).
pub(crate) mod agg;

/// Why a change to a pipeline's rows is visible to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The rows reach no aggregate: they are the output, or a consumer keeps their order.
    Rows,
    /// A LIMIT / OFFSET keeps PostgreSQL's first rows (D1804).
    Limit,
    /// A row lock: which rows, in what order (V4R-512).
    Lock,
    /// An aggregate whose result reads the row order (D853: a float sum; an ordered or kept aggregate: `graph::order_free`).
    Agg,
    /// min / max keep PostgreSQL's member among equal values on this walk (D862: `graph::walk_free`).
    Member,
    /// A fold that may raise by its partial sums with no guard of its own (D2597: `fold_visible`): another order can
    /// hide PostgreSQL's overflow.
    FoldError,
    /// A VOLATILE function or operator, or a nested plan driven per row that calls one (D831: a NOTICE's, a write's or
    /// nextval's order and count are PostgreSQL's to see).
    Volatile,
    /// An outer-first INNER / SEMI hash probe reached through ops that may raise (D2593: over an empty table PG reads the
    /// outer's first row alone, so which row is first decides the error).
    FirstRow,
    /// A VOLATILE call a rewind of the statement would make again where PostgreSQL's rescan keeps its result (D2651).
    Rewind,
}

/// core-face's effect tests (user-code reach by OID, a set's pool safety, a SQL / PL body).
pub(crate) mod core_face;

/// Whether the order in which pipeline `k`'s source produces rows is visible: `None` only under an aggregate whose result
/// no row order changes, with no LIMIT / OFFSET or row lock, no visible call and no visible error.
pub fn order_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    visible_by(gx, k, true)
}

/// `order_visible` for a pipeline that runs on many workers (D3085: a nest step's outer, ops/nestloop/widen.rs): the same
/// readings without the printed-member one (`Why::Member`), and a sort or top-k end reads as order-free too. Which member
/// equal under the type's equality a group key or a min / max shows is free at any width (the equal-member ruling: numeric
/// dscale, bpchar blanks, collation-equal strings), and a wide hashed group keeps whichever member its workers' merge meets
/// first in any case. A sort orders the rows itself, its ties in any order at every width (ops/sort raw.rs's unstable
/// sort), and takes every row before it hands one on, so neither the run's reader nor the client sees the order the
/// pipeline met them in; drain.rs `order_free` reads it so for a drain's end. `walk_free`'s member rule stays with
/// `order_visible`'s callers, which keep PostgreSQL's printed member at one width (heap_pages.rs's reverse walk).
pub fn wide_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    visible_by(gx, k, false)
}

/// D4532: `wide_visible`'s reading for a pipeline that ends in a hash build (a nest step's outer over a filled spool,
/// ops/nestloop/widen.rs `spool`): `None` when its end is a HashBuild and it holds no LIMIT / OFFSET or row lock, makes no
/// visible call (`calls_visible`) and no first-row error (`joins_quiet`'s reading, as `wide_visible`'s). The build takes
/// every row its pipeline hands it before any reader reads one, and the order its records come in is free: PostgreSQL's
/// Hash hands its child's rows to a table whose readers meet them in its bucket order (decompose facts/order.rs
/// `order_free_under`: a Hash's child is order-free), which a build run on many workers changes already; a build whose
/// record order a reader keeps (nest.eqkey's keyed form under a parent that reads the Nest's order) is marked serial by
/// decompose (`SortedSource`), so its pipeline runs on the leader whatever its steps say.
pub fn wide_build_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    for op in gx.pipelines()[k].ops.iter() {
        match op {
            Op::Limit { .. } => return Some(Why::Limit),
            Op::Lock(_) => return Some(Why::Lock),
            _ => {}
        }
    }
    if crate::frame::iface::end_breaker(gx, k) != Some(gravity_abi::breaker::BreakerKind::HashBuild) {
        return Some(Why::Rows);
    }
    calls_visible(gx, k).or_else(|| error_by(gx, k, true))
}

/// `order_visible`, reading the printed member (`walk_free`) only when `member`; `!member` (`wide_visible`) also takes a
/// sort or top-k end with no visible call or first-row error whose run no reader folds by its ties' order (`run_visible`).
fn visible_by(gx: &Gx<'_, '_>, k: usize, member: bool) -> Option<Why> {
    for op in gx.pipelines()[k].ops.iter() {
        match op {
            Op::Limit { .. } => return Some(Why::Limit),
            Op::Lock(_) => return Some(Why::Lock),
            _ => {}
        }
    }
    let sorts = matches!(crate::frame::iface::end_breaker(gx, k), Some(gravity_abi::breaker::BreakerKind::Sort | gravity_abi::breaker::BreakerKind::TopK));
    if !member && sorts {
        return calls_visible(gx, k).or_else(|| error_by(gx, k, true)).or_else(|| run_visible(gx, k));
    }
    let Some(g) = gx.group_at(k) else { return Some(Why::Rows) };
    if !crate::graph::order_free(g) {
        return Some(Why::Agg);
    }
    if let Some(w) = g.aggs.iter().find_map(fold_visible).or_else(|| calls_visible(gx, k)).or_else(|| error_by(gx, k, !member)) {
        return Some(w);
    }
    (member && !crate::graph::walk_free(g)).then_some(Why::Member)
}

/// D3085: whether a reader of pipeline `k`'s sorted run sees the order its ties come in: a group fed by the run (a streamed
/// group in a reader, or the group a reader builds) whose result depends on it (an order-sensitive aggregate with no ORDER BY
/// of its own, as string_agg / array_agg / json_agg or a user aggregate keeping a first or last row, or a pinned first row:
/// `graph::order_free`) or whose fold may raise by its partial sums' order (`fold_visible`); or a reader that hands the run
/// on to any other breaker. A reader into the client's sink or another sort, and a group of order-free folds, see none:
/// equal sort keys come in any order at every width (ORDER BY ties and LIMIT ties are free). No reader found reads as
/// visible.
fn run_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    use gravity_abi::breaker::BreakerKind as B;
    let Some(run) = crate::frame::iface::end_breaker_of(gx, k) else { return Some(Why::Rows) };
    let ps = gx.pipelines();
    let mut readers = (k + 1..ps.len()).filter(|&q| gx.life_of(q, run.slot).is_some_and(|l| l.0 as usize == k)).peekable();
    if readers.peek().is_none() {
        return Some(Why::Rows);
    }
    let group = |g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>| if !crate::graph::order_free(g) { Some(Why::Agg) } else { g.aggs.iter().find_map(fold_visible) };
    for q in readers {
        let streamed = ps[q].ops.iter().filter_map(|op| match op {
            Op::GroupRun(i) => gx.stream(*i as usize),
            _ => None,
        });
        if let Some(w) = streamed.map(group).find(Option::is_some) {
            return w;
        }
        match crate::frame::iface::end_breaker(gx, q) {
            None | Some(B::Sort | B::TopK) => {}
            Some(_) => match gx.group_at(q) {
                Some(g) => {
                    if let Some(w) = group(g) {
                        return Some(w);
                    }
                }
                None => return Some(Why::Rows),
            },
        }
    }
    None
}

/// Whether pipeline `k` makes a VOLATILE call whose order or count the user sees: in its quals, projections, probe keys and
/// quals, sets or aggregate arguments and FILTERs, or in a nested plan it drives per row (a per-row SubPlan's plan, test
/// and args, a nest inner and its join qual; decompose F5, D1336). A function the catalog cannot answer counts as one.
pub fn calls_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    calls_by_ops(gx, k, true)
}

/// `calls_visible`, reading a hashed SubPlan's build op (`SubForm::Build`, which runs nothing: lowerv4 ops/subplan
/// hashed.rs `Pass`) by its facts when `build` holds. Those facts are the whole call's (its test, args and plan: decompose
/// rerun.rs `of_subplan`), and only its plan runs in the fill, its calls the fill's own ops and source; so with `build`
/// false a build op whose test or args make a VOLATILE call (which the probe makes, per row) counts only when the fill's
/// source may make one (a VALUES list), and one whose test and args make none counts by its facts (the plan's).
fn calls_by_ops(gx: &Gx<'_, '_>, k: usize, build: bool) -> Option<Why> {
    use gravity_decompose::ops::shared::subplan::SubForm;
    let v = |n: Node<'_>| calls(gx, n);
    let p = &gx.pipelines()[k];
    let source = matches!(p.source, gravity_decompose::graph::Source::Values { lists, .. } if lists.iter().any(v));
    let probe_calls = |d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>| d.test.is_some_and(v) || d.args.iter().any(v);
    let ops = p.ops.iter().any(|op| match op {
        Op::SubPlan(d) if !build && matches!(d.form, SubForm::Build) => d.volatility == Volatility::Volatile && (source || !probe_calls(d)),
        Op::Filter(n) | Op::Gate(n) => v(*n),
        Op::Project(l) => l.iter().any(v),
        Op::Join(JoinDesc::Probe { keys, qual, clauses, .. }) => keys.iter().chain(qual.iter()).chain(clauses.iter()).any(v),
        Op::Srf(d) => d.functions.iter().any(|f| v(f.node)),
        Op::Join(JoinDesc::Nest { qual, facts, .. }) => facts.volatility == Volatility::Volatile || qual.iter().any(v),
        Op::SubPlan(d) => d.volatility == Volatility::Volatile,
        _ => false,
    });
    (ops || gx.group_at(k).is_some_and(|g| g.aggs.iter().any(|a| v(a.node) || a.filter.is_some_and(v)))).then_some(Why::Volatile)
}

/// D4124: whether pipeline `k`'s own expressions (`calls_by_ops`' quals, projections, probe keys and quals, sets, a nest's
/// join qual, aggregate arguments and FILTERs, a VALUES source) make a VOLATILE call whose place among the statement's
/// calls shows in a value (`volatile::Reading::order`: a sequence or clock call, one that reads or sets their state, user
/// code). PG's Append fixes that place between its arms (each arm whole in turn). D4144: a SubPlan step counts by the one
/// SubPlan answer's `order_call` (decompose `SubPlanDesc::answer`: the same calls, read over its plan, test and args), which
/// a data-modifying CTE's write does not set (it reads VOLATILE, yet every read of the statement shares one snapshot and
/// sees none of it: its order PG leaves unpredictable). A nest inner's facts are not read. A builtin that reads server
/// state and a random draw show no place either.
pub fn order_shown(gx: &Gx<'_, '_>, k: usize) -> bool {
    let v = |n: Node<'_>| super::volatile::walk(gx, n, &super::volatile::Reading::order());
    let p = &gx.pipelines()[k];
    let source = matches!(p.source, gravity_decompose::graph::Source::Values { lists, .. } if lists.iter().any(v));
    let ops = p.ops.iter().any(|op| match op {
        Op::Filter(n) | Op::Gate(n) => v(*n),
        Op::Project(l) => l.iter().any(v),
        Op::Join(JoinDesc::Probe { keys, qual, clauses, .. }) => keys.iter().chain(qual.iter()).chain(clauses.iter()).any(v),
        Op::Srf(d) => d.functions.iter().any(|f| v(f.node)),
        Op::Join(JoinDesc::Nest { qual, .. }) => qual.iter().any(v),
        Op::SubPlan(d) => d.answer().order_call,
        _ => false,
    });
    source || ops || gx.group_at(k).is_some_and(|g| g.aggs.iter().any(|a| v(a.node) || a.filter.is_some_and(v)))
}

/// Whether expression `n` makes a VOLATILE call: the union of the VOLATILE readings (T2; effects/volatile.rs `any`: a call
/// D1871's nodes make whose function pg_proc marks VOLATILE or cannot answer, and (D1899) a cast to a domain whose CHECK
/// makes one), so does a range function over a range whose comparison is user code (D2065, K2).
fn calls(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    calls_by(gx, n, RangeCmp::Hidden)
}

/// How `calls_by` reads a builtin range function over a range whose comparison is user code (effects.rs
/// `range_user_cmp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeCmp {
    /// The comparator's calls count as VOLATILE calls (fail closed): the builtin runs them where no expression shows them.
    Hidden,
    /// The caller evaluates the expression where PostgreSQL does, once per candidate as PostgreSQL's node does, so the
    /// comparator's calls are neither hidden nor moved: a hash clause over a range keyed in its bucket form
    /// (visible/hashjoin.rs `bucket_pair`), which PostgreSQL runs on each hash-equal candidate (ExecScanHashBucket) and the
    /// lowering on each image-equal record. The number of those calls may differ; a VOLATILE call's count is free.
    InPlace,
}

/// `calls`, reading a range function over a user comparator as `range` says.
fn calls_by(gx: &Gx<'_, '_>, n: Node<'_>, range: RangeCmp) -> bool {
    gx.fx(n).volatile_by(&super::volatile::Reading::any()) || range == RangeCmp::Hidden && gx.fx(n).range_user_cmp()
}

/// Whether every VOLATILE call in expression `n` is an independent random draw (by the builtin it runs: `random()`,
/// `random(min, max)`, `random_normal()`, `gen_random_uuid()` / `uuidv4()`). Which group draws which value shows in no
/// result, so evaluating it over the groups in another order than PostgreSQL's hashed Agg emits them returns an order
/// PostgreSQL could have produced: that output order is plan-dependent (nodeAgg.c `agg_retrieve_hash_table` walks the
/// hash table), and any VOLATILE order PostgreSQL could produce passes. A sequence (`nextval`: its values number the
/// calls), a clock, a user function or one the catalog cannot answer shows its call's place among the groups, and no
/// plan-only setting makes PostgreSQL's group order Gravity's: none of those is blind.
/// Known limit, accepted: after `setseed()` the `random()` family's draws are a fixed sequence (pseudorandomfuncs.c:
/// one `prng_state` that `setseed` seeds and every draw reads; `gen_random_uuid` uses pg_strong_random), so the k-th
/// call gets the k-th value. Which group makes the k-th call still follows the hashed Agg's plan-dependent order, so a
/// seeded statement's groups may take other values than PostgreSQL's run gave them; no result pins that order, so it
/// stays blind.
pub fn call_order_blind(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    !super::volatile::walk(gx, n, &super::volatile::Reading::draw()) && call_order_free(gx, n)
}

/// ORDER-FREE: where PostgreSQL's own order of `n`'s calls comes out of a hash table (a hashed Agg's groups), Gravity may
/// make the calls in its own order (any ordering of the calls' multiset; their count is free under the count rule: each
/// used row at least once), whatever a value's dependence on its call's place (a counting user function, a sequence
/// draw, a clock). Every FuncExpr / OpExpr call of `n` is free there; a domain CHECK's VOLATILE calls (`volatile_check`)
/// and a user range comparator (`range_user_cmp`) keep `call_order_blind`'s reading. The caller decides that PostgreSQL's
/// order is a hash table's (topn's `backs`: a GroupForm::Hashed group without grouping sets); a sorted Agg's is not.
pub fn call_order_free(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    !super::volatile_check(gx, n) && !super::range_user_cmp(gx, n)
}

/// D2651: whether a rewind of the statement (ExecutorRewind, a SCROLL cursor's MOVE ABSOLUTE 0 or FETCH FIRST: every
/// node rescanned with no changed param) would make a VOLATILE call again whose result PostgreSQL keeps. Its rescan
/// replays what a breaker filled (a SCROLL portal's Sort is randomAccess, a hashed Agg or SetOp and a one-batch Hash
/// keep their tables, a Material its rows), rewinds an uncorrelated FunctionScan's stored rows and an uncorrelated
/// SubPlan's or unparameterized nest inner's kept fill, where the served rewind runs every pipeline again. So: a
/// pipeline other than the root (it fills a breaker) and outside a rerun set PG drives again (its params change) whose
/// calls are visible (`calls_visible`), or anywhere a FROM
/// function called with a VOLATILE expression on arguments that read no row (a LATERAL one over the outer row is called
/// again per row, its params changed), a SubPlan with no per-row argument or a nest inner with no NestLoopParam that
/// makes a VOLATILE call; never in an InitPlan or a pipeline filling one (a rewind keeps their values: D2647). The
/// root's own expressions PostgreSQL evaluates again too (a VOLATILE target list postponed past the Sort), as it drives
/// a correlated SubPlan or a parameterized inner again per row (its params change). Over-approximate on purpose: a
/// re-executed fill (a plain Agg's input) is counted too, and the entry's answer is the row executor's.
pub fn rewind_visible(gx: &Gx<'_, '_>) -> Option<Why> {
    let ps = gx.pipelines();
    let mut init = mcx::pg_vec![in gx.mcx; false; ps.len()];
    for i in gx.graph.init_plans.iter() {
        fill(gx, i.pipeline as usize, &mut init);
    }
    let kept = |op: &Op<'_>| kept_call(gx, op);
    // a rerun set's member is run again at each re-open of its owner (decompose `Rerun`: its params change), unless PG
    // keeps it at a rescan (`PipelineGraph::kept`)
    let rerun = |k: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16))) && !gx.graph.kept.contains(&(k as u16));
    let root = gx.graph.root as usize;
    // a pipeline ending in the sink (an Append arm streamed to the statement's output) is the top chain: PG's rescan
    // evaluates it again, as the root's
    let fills = |k: usize| k != root && ps[k].desc.produces().is_some() && !rerun(k) && calls_by_ops(gx, k, false).is_some();
    (0..ps.len()).filter(|&k| !init[k]).any(|k| ps[k].ops.iter().any(kept) || fills(k)).then_some(Why::Rewind)
}

/// D2651: op `op` makes a VOLATILE call whose result PostgreSQL's rescan keeps: a FROM function called on arguments that
/// read no row (its stored rows rewound), a SubPlan with no per-row argument or a nest inner with no NestLoopParam. Not
/// a hashed SubPlan's probe or build (D3377): PostgreSQL's rescan keeps its table but evaluates its test again per row
/// (ExecHashSubPlan projects the left side and probes), as the served rewind does; its table's fill is a pipeline
/// `rewind_visible`'s `fills` reads by its own ops (the build op's facts are the whole call's, its test's too).
fn kept_call(gx: &Gx<'_, '_>, op: &Op<'_>) -> bool {
    use gravity_decompose::ops::shared::subplan::SubForm;
    match op {
        Op::Srf(d) => matches!(d.place, SrfPlace::From { .. }) && d.functions.iter().any(|f| calls(gx, f.node) && !f.args.iter().any(reads_row)),
        Op::SubPlan(d) => d.args.len() == 0 && d.volatility == Volatility::Volatile && matches!(d.form, SubForm::Nested { .. }),
        Op::Join(JoinDesc::Nest { params, facts, .. }) => params.len() == 0 && facts.volatility == Volatility::Volatile,
        _ => false,
    }
}

/// D2701: whether a rewind that keeps the live execution and runs only the emits (the root, and each Append arm streamed
/// to the sink) again equals PostgreSQL's rescan. PostgreSQL rewinds the executor only for a backward-scannable plan (one
/// it would put a Material over replays from the portal's store: D1406), whose top chain is scans, Result, Limit,
/// SubqueryScan, Append, LockRows, a Sort (randomAccess under the portal's EXEC_FLAG_REWIND, which Append passes to every
/// arm), a Material, a FunctionScan and a CteScan: its rescan evaluates the chain again (a correlated SubPlan per row)
/// and keeps the Sort's and the Material's rows, a FunctionScan's stored rows, the CTE store and the InitPlans' values.
/// The kept execution holds every filled slot, the rerun sets' state (a RERUN_KEPT member's fill, which PG keeps too) and
/// the InitPlan cells. So it is exact when no pipeline it runs again makes a call PG keeps the result of (`kept_call`),
/// and every fill an emit reads through other than its own Sort, top-k or Material input, which PG may execute again,
/// makes no visible call (its kept rows are then the same). D3377 (initplan_d2721 A5): an Append's arms are emits, run
/// again in arm order as PostgreSQL's ExecReScanAppend starts over at its first arm; the re-arm runs the program's emits
/// from the first one on, so they must be its last pipelines (a fill between two would run again: driver entry.rs
/// `rewind_emit`).
pub fn soft_rewind(gx: &Gx<'_, '_>) -> bool {
    use gravity_abi::breaker::BreakerKind as B;
    let ps = gx.pipelines();
    let mut init = mcx::pg_vec![in gx.mcx; false; ps.len()];
    for i in gx.graph.init_plans.iter() {
        fill(gx, i.pipeline as usize, &mut init);
    }
    let member = |k: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)));
    // the emits: the root and every Append arm streamed to the sink (no slot filled, no InitPlan's cell); the re-arm runs
    // more than one again from the first on, so then every pipeline from there is one
    let emit = |k: usize| !init[k] && ps[k].desc.produces().is_none();
    let mut todo: PgVec<'_, usize> = (0..ps.len()).filter(|&k| emit(k)).collect_in(gx.mcx);
    if todo.first().is_none_or(|&first| todo.len() > 1 && todo.len() != ps.len() - first) {
        return false;
    }
    let mut seen = mcx::pg_vec![in gx.mcx; false; ps.len()];
    while let Some(p) = todo.pop() {
        if p >= ps.len() || core::mem::replace(&mut seen[p], true) {
            continue;
        }
        // a set function an emit runs ends at a rewind (ReScanExprContext's callbacks, D703): the re-arm keeps it
        let srf = emit(p) && ps[p].ops.iter().any(|op| matches!(op, Op::Srf(_)));
        if srf || ps[p].ops.iter().any(|op| kept_call(gx, op)) || (!emit(p) && calls_visible(gx, p).is_some()) {
            return false;
        }
        for &s in ps[p].desc.consumes.iter() {
            let mut met = false;
            for q in (0..p).rev() {
                if ps[q].desc.produces() == Some(s) {
                    met = true;
                    let kind = gx.graph.breakers.iter().find(|b| b.breaker.slot == s && b.producer as usize == q).map(|b| b.breaker.kind);
                    let keeps = matches!(kind, Some(B::Materialize)) || (emit(p) && matches!(kind, Some(B::Sort | B::TopK)));
                    if !init[q] && !member(q) && !keeps {
                        todo.push_in(q);
                    }
                } else if met && ps[q].desc.consumes.contains(&s) {
                    break;
                }
            }
        }
    }
    true
}

/// D439: the program evaluates a VOLATILE call per row: some op declared a `Volatile` protocol (an expression's VOLATILE
/// call, a hosted aggregate's or a group's per-row call, a rerun store or key that calls one). A pull then asks for the
/// rows the fetch still owes, never a full block (`V4Descs::pull_exact`; the driver adds a row lock's).
pub fn pull_exact(protos: &[gravity_program::protos::Proto]) -> bool {
    protos.iter().any(|p| p.kind == gravity_program::protos::ProtoKind::Volatile)
}

/// D2651 (the KEPT road): does a rewind of this SCROLL plan make no call a reader can see, so that replaying the portal's
/// cursor store equals PostgreSQL's rescan? ExecReScan from the top with no changed param re-evaluates only the top
/// chain (Limit / Result over an outer plan / SubqueryScan / Append / the scans): its target lists, quals, Limit counts,
/// Result's constant qual, index keys, TID quals and VALUES lists, and a correlated SubPlan there runs its plan again per
/// row. The Sort under the portal's EXEC_FLAG_REWIND (randomAccess), a Material, a FunctionScan's stored rows, a CteScan's
/// CTE store and the InitPlans keep what they hold (ExecReScanSort / Material / FunctionScan / CteScan, ExecReScan's
/// initPlan arm). So when the chain is quiet the rows a rewind reads again are the ones the store holds, then the
/// executor's next rows: no VOLATILE call under the kept node runs twice (nextval under the Sort, a VOLATILE FROM
/// function) and none the chain makes again is skipped. Quiet: whitelisted expression nodes, every function or operator
/// built-in (no user code: no NOTICE or table write a second call could show) and not VOLATILE, a built-in aggregate
/// without ORDER BY / DISTINCT, an uncorrelated hashed SubPlan (its table is kept, only its test runs again) or a SubPlan
/// whose per-call plan is quiet (`per_call_quiet`). Any other expression or plan node is loud. The admit entry asks it
/// only for the portals it serves (the row executor's rewinds keep D1406's answer).
pub fn rewind_quiet<'a>(pstmt: &types_nodes::plannodes::PlannedStmt<'a>, cat: &dyn Catalog<'a>) -> bool {
    chain_quiet(&pstmt.subplans, cat, pstmt.planTree)
}

pub(in crate::effects) type Subplans<'s, 'a> = &'s types_nodes::list::OptNodeList<'a>;

fn chain_quiet<'a>(sp: Subplans<'_, 'a>, cat: &dyn Catalog<'a>, node: Option<Node<'a>>) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    let Some(node) = node else { return false };
    let Some(plan) = node.as_plan() else { return false };
    if plan.parallel_aware || !list_quiet(sp, cat, &plan.targetlist) || !list_quiet(sp, cat, &plan.qual) {
        return false;
    }
    let q = |n: Node<'a>| PlanFx::of(cat, n).rewind_quiet(sp);
    match node.node_tag() {
        // their rescan keeps the rows (Sort / Material), the function results (FunctionScan) or the CTE store (CteScan)
        T::T_Sort | T::T_Material | T::T_FunctionScan | T::T_CteScan | T::T_SeqScan => true,
        T::T_Result => plan.lefttree.is_some() && node.as_result().is_some_and(|r| r.resconstantqual.is_none_or(q)) && chain_quiet(sp, cat, plan.lefttree),
        T::T_Limit => node.as_limit().is_some_and(|l| limit_quiet(sp, cat, l)) && chain_quiet(sp, cat, plan.lefttree),
        T::T_SubqueryScan => chain_quiet(sp, cat, node.as_subquery_scan().and_then(|s| s.subplan)),
        T::T_Append => node.as_append().is_some_and(|a| a.nasyncplans == 0 && a.appendplans.iter().all(|p| chain_quiet(sp, cat, Some(p)))),
        T::T_IndexScan | T::T_IndexOnlyScan | T::T_TidScan | T::T_TidRangeScan | T::T_ValuesScan => keys_quiet(sp, cat, node),
        _ => false,
    }
}

/// D3509 (Workers IO sig-4c3f50662a3c): does a rewind of this SCROLL plan read again rows PostgreSQL keeps, through a chain that
/// computes nothing again a reader could see change? Its rescan replays what a Sort (randomAccess under the portal's
/// EXEC_FLAG_REWIND), a Material, a FunctionScan or a CteScan holds and evaluates only the chain above that node again. A
/// served rewind runs the whole program again: a parallel sort may order its ties another way (another row at the same
/// cursor position), and a STABLE value under the kept node is computed again (a timestamptz::text after SET TimeZone).
/// Replaying the portal's cursor store is exact when the top chain (Result over an outer plan, Limit, SubqueryScan)
/// reaches such a node with only IMMUTABLE expressions (the home's `PlanFx::fixed`), so the chain's second evaluation equals
/// the first.
pub fn rewind_keeps<'a>(pstmt: &types_nodes::plannodes::PlannedStmt<'a>, cat: &dyn Catalog<'a>) -> bool {
    chain_keeps(cat, pstmt.planTree)
}

fn chain_keeps<'a>(cat: &dyn Catalog<'a>, node: Option<Node<'a>>) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    let Some(node) = node else { return false };
    let Some(plan) = node.as_plan() else { return false };
    let fixed = |l: &types_nodes::NodeList<'a>| l.iter().all(|n| PlanFx::of(cat, n).fixed());
    if plan.parallel_aware || !fixed(&plan.targetlist) || !fixed(&plan.qual) {
        return false;
    }
    let f = |n: Node<'a>| PlanFx::of(cat, n).fixed();
    match node.node_tag() {
        T::T_Sort | T::T_Material | T::T_FunctionScan | T::T_CteScan => true,
        T::T_Result => plan.lefttree.is_some() && node.as_result().is_some_and(|r| r.resconstantqual.is_none_or(f)) && chain_keeps(cat, plan.lefttree),
        T::T_Limit => {
            node.as_limit().is_some_and(|l| {
                l.limitOffset.is_none_or(f) && l.limitCount.is_none_or(f) && l.uniqOperators.iter().all(|&o| o != 0 && o < types_core::catalog::FirstNormalObjectId)
            }) && chain_keeps(cat, plan.lefttree)
        }
        T::T_SubqueryScan => chain_keeps(cat, node.as_subquery_scan().and_then(|s| s.subplan)),
        _ => false,
    }
}

fn limit_quiet<'a>(sp: Subplans<'_, 'a>, cat: &dyn Catalog<'a>, l: &types_nodes::plannodes::Limit<'a>) -> bool {
    let q = |n: Node<'a>| PlanFx::of(cat, n).rewind_quiet(sp);
    l.limitOffset.is_none_or(q) && l.limitCount.is_none_or(q) && l.uniqOperators.iter().all(|&o| o != 0 && o < types_core::catalog::FirstNormalObjectId)
}

/// The scan-specific expressions a rescan evaluates again (index keys, TID quals, VALUES lists).
fn keys_quiet<'a>(sp: Subplans<'_, 'a>, cat: &dyn Catalog<'a>, node: Node<'a>) -> bool {
    use types_nodes::NodeTag as T;
    let ql = |l: &types_nodes::NodeList<'a>| list_quiet(sp, cat, l);
    match node.node_tag() {
        T::T_IndexScan => node.as_index_scan().is_some_and(|s| [&s.indexqual, &s.indexqualorig, &s.indexorderby, &s.indexorderbyorig].into_iter().all(ql)),
        T::T_IndexOnlyScan => node.as_index_only_scan().is_some_and(|s| [&s.indexqual, &s.recheckqual, &s.indexorderby].into_iter().all(ql)),
        T::T_TidScan => node.as_tid_scan().is_some_and(|s| ql(&s.tidquals)),
        T::T_TidRangeScan => node.as_tid_range_scan().is_some_and(|s| ql(&s.tidrangequals)),
        T::T_ValuesScan => node.as_values_scan().is_some_and(|s| ql(&s.values_lists)),
        _ => false,
    }
}

/// A SubPlan's plan, run again at each call (ExecScanSubPlan rescans it with its params changed): every expression it
/// evaluates is quiet, but a FunctionScan's function that reads no PARAM_EXEC param keeps its stored rows
/// (ExecReScanFunctionScan drops them only when its `funcparams` changed). An InitPlan inside, or a node not named, is loud.
pub(in crate::effects) fn per_call_quiet<'a>(sp: Subplans<'_, 'a>, cat: &dyn Catalog<'a>, node: Option<Node<'a>>) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    let Some(node) = node else { return true };
    let Some(plan) = node.as_plan() else { return false };
    if plan.parallel_aware || plan.initPlan.len() != 0 || !list_quiet(sp, cat, &plan.targetlist) || !list_quiet(sp, cat, &plan.qual) {
        return false;
    }
    if !per_call_quiet(sp, cat, plan.lefttree) || !per_call_quiet(sp, cat, plan.righttree) {
        return false;
    }
    let q = |n: Node<'a>| PlanFx::of(cat, n).rewind_quiet(sp);
    match node.node_tag() {
        T::T_SeqScan | T::T_Sort | T::T_Material | T::T_Agg => true,
        T::T_Result => node.as_result().is_some_and(|r| r.resconstantqual.is_none_or(q)),
        T::T_Limit => node.as_limit().is_some_and(|l| limit_quiet(sp, cat, l)),
        T::T_FunctionScan => node.as_function_scan().is_some_and(|f| f.functions.iter().all(|r| r.as_range_tbl_function().is_some_and(|r| r.funcparams.is_empty() || r.funcexpr.is_none_or(q)))),
        T::T_SubqueryScan => per_call_quiet(sp, cat, node.as_subquery_scan().and_then(|s| s.subplan)),
        T::T_IndexScan | T::T_IndexOnlyScan | T::T_TidScan | T::T_TidRangeScan | T::T_ValuesScan => keys_quiet(sp, cat, node),
        _ => false,
    }
}

fn list_quiet<'a>(sp: Subplans<'_, 'a>, cat: &dyn Catalog<'a>, l: &types_nodes::NodeList<'a>) -> bool {
    l.iter().all(|n| PlanFx::of(cat, n).rewind_quiet(sp))
}

/// Whether expression `n` reads a row: a column, or a PARAM_EXEC param (a nest's or a SubPlan's, set per outer row).
fn reads_row(n: Node<'_>) -> bool {
    crate::stack_check();
    let mut any = n.as_var().is_some() || n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC);
    gravity_decompose::expr::kids(n, &mut |c| any |= reads_row(c));
    any
}

/// Marks pipeline `at` and every pipeline filling a slot it reads (the producers met walking back from it, up to an
/// earlier reader of the same slot number, as decompose rerun.rs walks a fill).
fn fill(gx: &Gx<'_, '_>, at: usize, out: &mut [bool]) {
    crate::stack_check();
    let ps = gx.pipelines();
    if at >= out.len() || out[at] {
        return;
    }
    out[at] = true;
    for &s in ps[at].desc.consumes.iter() {
        let mut met = false;
        for q in (0..at).rev() {
            if ps[q].desc.produces() == Some(s) {
                met = true;
                fill(gx, q, out);
            } else if met && ps[q].desc.consumes.contains(&s) {
                break;
            }
        }
    }
}

/// Whether which of pipeline `k`'s rows comes first decides an error the user sees: the pipeline reaches an outer-first
/// hash probe that drops its unmatched rows (INNER, SEMI: PG's HJ_BUILD_HASHTABLE fetches the outer's first row before it
/// builds) through ops that may raise. Over an EMPTY table PG reads that first row alone and ends the join (its filter's
/// error, or no error where a later row's would raise). A fill type (LEFT, FULL, ANTI) reads every outer row either way.
pub fn error_visible(gx: &Gx<'_, '_>, k: usize) -> Option<Why> {
    error_by(gx, k, false)
}

/// `error_visible`, reading the ops ahead of the probe as `joins_quiet` does when `joins` (`wide_visible`'s reading), else
/// as `ops_raise_free` does (a filter, gate or projection; any join op may raise).
fn error_by(gx: &Gx<'_, '_>, k: usize, joins: bool) -> Option<Why> {
    let ops = &gx.pipelines()[k].ops;
    let fill = |jt: &JoinType| matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_FULL | JoinType::JOIN_ANTI);
    let quiet = |pre: &[Op<'_>]| if joins { joins_quiet(gx, pre) } else { super::ops_raise_free(gx, pre) };
    ops.iter()
        .enumerate()
        .any(|(j, op)| matches!(op, Op::Join(JoinDesc::Probe { outer_first: true, jointype, .. }) if !fill(jointype)) && !quiet(&ops[..j]))
        .then_some(Why::FirstRow)
}

/// D3014 phase 3: whether every op of `ops`, the ops ahead of an outer-first probe in a pipeline that runs on many workers,
/// raises on no row. Over an empty build PostgreSQL reads the outer's first row alone, so an op ahead of the probe that
/// raises on no row shows nothing at any width however many rows the workers hand it. Filters, gates and projections read
/// as `ops_raise_free` reads them; an earlier hash probe raises on no row when its keys, join qual and clauses do, each
/// clause under a builtin hashable equality (no hash value of user code: decompose `effects::user_hash_clause`; drain.rs's `still`
/// reading); a nest step when its F5 facts, its join qual and its parameters do (`op_raise_free`'s tail reading). Any
/// other op may raise.
fn joins_quiet(gx: &Gx<'_, '_>, ops: &[Op<'_>]) -> bool {
    let clause = |c: Node<'_>| gx.fx(c).quiet() && !gravity_decompose::effects::user_hash_clause(gx.facts, c);
    ops.iter().all(|o| match o {
        Op::Join(JoinDesc::Probe { clauses, .. }) => super::op_raise_free(gx, o, Some(&[])) && clauses.iter().all(clause),
        Op::Join(JoinDesc::Nest { .. }) => super::op_raise_free(gx, o, Some(&[])),
        _ => super::op_raise_free(gx, o, None),
    })
}

/// D2597: whether fold `d`'s error depends on the row order beyond what errorhold's recheck repairs. A fold of a class
/// that may raise by its partial sums can hide PG's overflow when it meets the rows in another order (over 1e308, 1e308,
/// inf, -inf PG raises at 1e308 + 1e308; the reverse order meets -inf + inf first and sums to NaN with no error), which no
/// recheck sees, unless it raises wherever another order could. The float folds do (group#293: class_float's risk, the
/// float variances' `risky`, under `facts::order::rechecked`); money and interval sums and averages, PG's own transfns
/// hosted, have no such guard (decompose `effects::scan_raises`: PG's scan order decides `9e16 + 9e16 - 9e16`'s
/// money out of range), so they keep PostgreSQL's order: `Some` for them.
pub fn fold_visible(d: &AggDecl<'_>) -> Option<Why> {
    gravity_decompose::effects::scan_raises(d.class).then_some(Why::FoldError)
}

/// D2283: whether op `op`'s LIMIT / OFFSET (a Limit's, a top-N Sort's) can raise: PG's recompute_limits raises 2201W /
/// 2201X at the node's first call when a bound is negative. A bound no run makes negative (absent, NULL: decompose's
/// `Bound::All`, or a constant >= 0) never does; one known only at the run (a parameter, an expression) may. Any other
/// op: false (its expressions are asked apart).
pub fn bounds_may_raise(op: &Op<'_>) -> bool {
    use gravity_decompose::{ops::shared::sort::SortForm, Bound};
    let loose = |b: &Bound<'_>| !matches!(b, Bound::All | Bound::Const(0..));
    match op {
        Op::Limit { count, offset, .. } => loose(count) || loose(offset),
        Op::Sort(s) => matches!(&s.form, SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } if loose(count) || loose(offset)),
        _ => false,
    }
}

/// A LIMIT / OFFSET without WITH TIES whose bounds cannot raise: a fill's op past its Sort that PostgreSQL's Limit runs
/// over the sorted store only (rerun/spool/form.rs `first_sort`).
pub fn quiet_limit(op: &Op<'_>) -> bool {
    matches!(op, Op::Limit { ties: None, .. }) && !bounds_may_raise(op)
}

/// topn's moved decisions (EFF-TOPN: a top over group finals, its count cut and its two-pass split).
pub(crate) mod topn;

/// rep's key reach: whether a key type may reach user code behind its comparator.
pub(crate) mod rep;

/// Comparisons read quiet past PostgreSQL's leakproof mark (a hash join drain arm's comparison, D393).
pub(crate) mod errorhold;

/// The sort family's decisions: a Sort's served raise at its first call, and finals that run once per group.
pub(crate) mod sort;

/// What the nodes PostgreSQL evaluates before a per-row SubPlan's read may do (per_row.rs `before`).
pub(crate) mod subplan;

/// A pulled run's MJ-AHEAD test: whether the pulled pipeline's code may run ahead of its reader (face/pull.rs).
pub(crate) mod pull;

/// A settled union's calm and raise-only reads (rerun/union_reads.rs's D1486 / D3373 rules).
pub(crate) mod union;

/// The rerun family's calm arm, calm write and user-code key (rerun/inner_probe.rs, spool/form.rs, params_key.rs).
pub(crate) mod rerun;
