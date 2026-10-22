//! hashjoin's effects questions, moved as they were from ops/hashjoin, so the family asks the effects home: the statement-wide quiet reading and its in-row guard
//! (build_class.rs's side class reads them), a raising CTE build's order, the outer keys' quiet test, the calls the side
//! class does not see, a set function's quiet reading, a CTE InitPlan's loud facts, and the join.mult road's two tests
//! (strategies/multiplicity.rs: whether the ops after an inner probe could tell a row's copies apart).
use gravity_decompose::effects::Volatility;
use gravity_decompose::catalog::IndexAm;
use gravity_decompose::graph::Source;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::Op;
use types_nodes::Node;

use crate::effects::{bitmap_all, initdb_index, user_typed, volatile};
use crate::graph::Gx;
use crate::ops::hashjoin::build_class::{feed, shared_cte, Side};

/// expr.volatile: a VOLATILE call past probe `j` of pipeline `k` (its ops, the group its end folds into) runs once per
/// match, as PG evaluates it on each joined row: never once for a row's copies. A call is one by the shared fail-closed
/// check (visible.rs `calls`: a function the catalog cannot answer, a domain CHECK's call (D1899), a range function over
/// a user comparator (D2065)) or by `effects::volatile` (every operator node, D1871).
pub(crate) fn past_volatile(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let v = |n: Node<'_>| super::calls(gx, n) || gx.fx(n).volatile_any();
    let ops = gx.pipelines()[k].ops[j + 1..].iter().any(|op| match op {
        Op::Filter(n) | Op::Gate(n) => v(*n),
        Op::Project(l) => l.iter().any(v),
        _ => false,
    });
    ops || gx.group_at(k).is_none_or(|g| g.keys.iter().any(|x| v(x.expr.node())) || g.aggs.iter().any(|a| v(a.node) || a.filter.is_some_and(v)))
}

/// Every copy of a payload-free row reads `n` alike: every node kind is one the walker sees through. A STABLE or IMMUTABLE
/// user function or operator in it does not act (DECISIONS 3, C1: it may run any number of times, only its errors are
/// judged, and each copy raises as the first does), so it no longer keeps join.mult off (gate audit AGA-05: was
/// builtin-only); a VOLATILE call is `past_volatile`'s, fail closed.
pub(crate) fn copies_alike(_gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gravity_decompose::expr::walk(n, &mut |_| true)
}

/// D1005 / D237: whether `n` calls code that makes a hash join side a user function's (PostgreSQL never makes the calls
/// of a side it never runs, so no deferral may run them): a non-builtin function or operator (`effects::user_fn`), or a
/// VOLATILE call by the shared check: `calls` (fail closed: a function the catalog cannot answer counts; a cast to a domain
/// whose CHECK makes one; a range function over a user comparator) or the union of the VOLATILE readings
/// (`volatile::Reading::any`: every operator node, D1871: IS DISTINCT FROM, NULLIF, a scalar-array op's functions).
pub(crate) fn side_calls(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    side_calls_by(gx, n, super::RangeCmp::Hidden, &volatile::Reading::any())
}

/// D3587 (effects.rs `hides_calls`): `side_calls` with a user function LANGUAGE internal over a builtin C function read
/// as that builtin, PostgreSQL's own code (user_fn `Reading::calls_body`).
pub(crate) fn side_calls_body(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).user_by(&crate::effects::user_fn::Reading::calls_body()) || super::calls_by(gx, n, super::RangeCmp::Hidden) || gx.fx(n).volatile_by(&volatile::Reading::any())
}

/// D3816 (`acting_side`, `Px::run_ahead`'s acting rule): `side_calls` counting the VOLATILE calls that act (volatile.rs
/// `Reading::acting`, decompose `effects::call_acts`): a pinned non-acting builtin's call (a disk size, the lock status)
/// reads as builtin code: it writes and holds nothing, so one made ahead of PostgreSQL's, or once more in the plan-order
/// recheck, changes no row.
pub(crate) fn side_acts(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    side_calls_by(gx, n, super::RangeCmp::Hidden, &volatile::Reading::acting())
}

/// `side_calls`, reading a range function over a user comparator as `range` says (visible.rs `RangeCmp`) and a VOLATILE
/// call as `reading` does.
fn side_calls_by(gx: &Gx<'_, '_>, n: Node<'_>, range: super::RangeCmp, reading: &volatile::Reading<'_>) -> bool {
    // visible.rs `calls_by` with its VOLATILE walk read by `reading`: a range function over a user comparator counts as `range` says
    gx.fx(n).user_calls() || gx.fx(n).volatile_by(reading) || range == super::RangeCmp::Hidden && gx.fx(n).range_user_cmp()
}

/// W4b-RowX: no pipeline of the statement is `loud`: the statement holds no user code that no expression shows, so
/// `side_by` may read what PG 18.6 runs without raising as quiet (expression by expression: an expression holding a value
/// of a user type or a row-reading xml builtin reads as before; the statement-wide reading was narrowed to the
/// expression), and D1904's later-call road (probe_calls.rs `rescans`) may run.
pub(crate) fn quiet_statement(gx: &Gx<'_, '_>) -> bool {
    // D3585: answered once per lowering of the graph under one `Gx` (memo.rs): asked per pipeline and op, each walk of
    // every pipeline's expressions made the lowering quadratic in an Append's arms
    let key = crate::memo::key_of(&gx.graph.pipelines);
    let at = gx_key(gx);
    if let Some(Some((k, quiet))) = crate::memo::read(key, |m| m.quiet) {
        if k == at {
            return quiet;
        }
    }
    let uf = |n: Node<'_>| side_calls(gx, n);
    let quiet = !gx.pipelines().iter().chain(gx.pipelines().inners().into_iter().map(|x| x.1)).any(|p| loud(gx, p, &uf, None));
    crate::memo::write(key, |m| m.quiet = Some((at, quiet)));
    quiet
}

/// D3585: what `quiet_statement` reads besides the graph (the overlay, the facts, the catalog), by address: a kept answer
/// is reused only under the same.
fn gx_key(gx: &Gx<'_, '_>) -> (usize, usize, usize) {
    let over = gx.over.map_or(0, |o| o as *const _ as usize);
    (over, gx.facts as *const _ as *const () as usize, gx.catalog as *const _ as *const () as usize)
}

/// D3374 (probe_calls.rs `rescans`, D1904's later-call road): `quiet_statement` with the hash clauses of probe op `j` of
/// pipeline `q` read by their operators alone. Their outer keys are the outer side that road serves by name (`side_by`
/// with its keys: PG computes each outer row's keys at every rescan over the kept table) and their inner keys the
/// build's (`side_by` of the build reads them), so a user or VOLATILE call in a key is shown; a user operator still counts.
pub(crate) fn quiet_statement_at(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    let uf = |n: Node<'_>| side_calls(gx, n);
    let ps = gx.pipelines();
    let at = |k: usize| (k == q).then_some(j);
    !(ps.iter().enumerate().any(|(k, p)| loud(gx, p, &uf, at(k))) || ps.inners().into_iter().enumerate().any(|(i, x)| loud(gx, x.1, &uf, at(ps.len() + i))))
}

/// D3609 (probe_calls.rs `orders`, D1902's `Order` with its close): pipeline `k`'s source is an index walk whose only
/// part that may raise is its keys, evaluated once at its seek, as PG evaluates an Index Scan's runtime keys at its first
/// fetch after a rescan (ExecIndexEvalRuntimeKeys): a btree over initdb opclasses (D2063's facts), keys that call no user
/// function, in a statement whose user code every expression shows (`quiet_statement`). A btree's recheck (the original
/// index quals, the raising key among them) is never evaluated (D2900: a btree entry is exact).
pub(crate) fn seek_raises_only(gx: &Gx<'_, '_>, k: usize) -> bool {
    match &gx.pipelines()[k].source {
        Source::Index { keys, indexid, iam: IndexAm::Btree, .. } => quiet_statement(gx) && initdb_index(gx, *indexid, keys) && keys.iter().all(|x| !side_calls(gx, x.value)),
        _ => false,
    }
}

/// W4b-RowX's in-row guard, narrowed to code no expression of the statement shows: an index or a bitmap whose key column's operator class or key operator is not an initdb one that cannot raise
/// (a user opclass's FUNCTION 1: D2063's facts, effects.rs `initdb_index`), a hash, merge or memo comparison that calls a
/// user or VOLATILE operator (`uf`) or compares a value of a user type (`user_typed`: v4 compares by the key's type, where
/// PG runs the type's functions: D2036, D2065, D2068), or a named tuplestore, TABLESAMPLE or table function source.
/// `shown`: the op whose hash clauses count by their operators alone (`quiet_statement_at`).
fn loud(gx: &Gx<'_, '_>, p: &gravity_decompose::PipelineNode<'_>, uf: &dyn Fn(Node<'_>) -> bool, shown: Option<usize>) -> bool {
    let src = match &p.source {
        Source::Index { indexid, keys, .. } => !initdb_index(gx, *indexid, keys),
        Source::Bitmap { plan, .. } => !bitmap_all(plan, &|i, k, _| initdb_index(gx, i, k)),
        Source::Values { .. } | Source::Tid { .. } | Source::Range { .. } | Source::Const | Source::Breaker { .. } => false,
        Source::Named { .. } | Source::Sample { .. } | Source::TableFunc { .. } => true,
    };
    let key = |n: Node<'_>| user_typed(gx, n) && !bucket_key(gx, n);
    let builtin = |c: Node<'_>| gx.fx(c).builtin_op();
    src || p.ops.iter().enumerate().any(|(i, op)| match op {
        Op::Join(JoinDesc::Build { keys: l, .. }) => l.iter().any(key),
        Op::Join(JoinDesc::Memo { keys: l, .. }) => l.iter().any(|n| user_typed(gx, n)),
        // a bucket clause runs its range comparator at the probe, where PostgreSQL's bucket scan runs it (`bucket_key`).
        // `uf` is `side_calls`, which InPlace's answer implies, so the clause's keys are read only when its one call is a
        // range comparator
        Op::Join(JoinDesc::Probe { clauses, keys, .. }) => clauses.iter().any(|c| (if shown == Some(i) { !builtin(c) } else { uf(c) }) && (side_calls_by(gx, c, super::RangeCmp::InPlace, &volatile::Reading::any()) || !bucket_pair(gx, c))) || clauses.iter().any(|c| user_typed(gx, c) && !bucket_pair(gx, c)) || keys.iter().any(key),
        Op::Join(JoinDesc::Merge { clauses, .. }) => clauses.iter().any(|c| uf(c) || user_typed(gx, c)),
        _ => false,
    })
}

/// K2.kh1e / kh1x (the range-key form, K2.hj): a hash key PG 18.6 hashes and compares through no code v4 does not run: a
/// range keyed by its bucket form (keys.rs `bucket`: its subtype's own form, built in), read as a column, a parameter or an
/// element of an array column or parameter at subscripts of no user type. PG hashes it by hash_range (the subtype's
/// default hash function, built in) and compares it by its probe's bucket clause (`bucket_pair`), which v4 runs on each
/// image-equal record as PG's bucket scan runs it on each hash-equal candidate (keys.rs `confirms`): no call is hidden.
fn bucket_key(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    let plain = |x: Node<'_>| x.as_var().is_some() || x.as_param().is_some();
    let read = plain(n)
        || n.as_relabel_type().is_some_and(|r| plain(r.arg))
        || n.as_subscripting_ref().is_some_and(|s| s.refexpr.is_some_and(plain) && s.refassgnexpr.is_none() && s.refupperindexpr.iter().chain(s.reflowerindexpr.iter()).flatten().all(|i| !user_typed(gx, i)));
    read && gravity_decompose::catalog::expr_type::type_of(n).is_some_and(|t| crate::ops::hashjoin::keys::bucket(t).is_some())
}

/// Hash clause `c` is the bucket clause (keys.rs `bucket_clause`: the range's own `=`) over two `bucket_key`s.
fn bucket_pair(gx: &Gx<'_, '_>, c: Node<'_>) -> bool {
    crate::ops::hashjoin::keys::bucket_clause(c) && c.as_op_expr().is_some_and(|o| o.args.iter().all(|a| bucket_key(gx, a)))
}

/// `shared_cte` for a build side that can raise and calls nothing (a self join of a CTE that may raise): the CTE runs whole
/// in either order, so the eager build raises the error of the first raising CTE row, PG's, before any row reaches the
/// probe. The builds of `q`'s probes before `j` are quiet, so no error PG raises at `q`'s first row (an earlier
/// outer-first build's) comes before the CTE's.
pub(crate) fn shared_cte_raises(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> bool {
    let quiet = |op: &Op<'_>| match op {
        Op::Join(JoinDesc::Probe { slot, .. }) => gx.life_of(q, *slot).map(|l| l.0 as usize).is_some_and(|x| x < gx.pipelines().len() && side(gx, x) == Side::Safe),
        _ => true,
    };
    shared_cte(gx, b, q, j) && gx.pipelines()[q].ops[..j].iter().all(quiet)
}

/// D1908: probe `op`'s outer hash keys raise nothing and call nothing (rtfilter.rs `raise_free`: PG marks no VOLATILE builtin
/// leakproof). PG's HJ_FILL_OUTER scan (LEFT, FULL, ANTI) hashes every outer row whatever the table holds, an empty one too
/// (nodeHashjoin.c ExecHashJoinOuterGetTuple); the probe may skip quiet keys there.
pub(crate) fn keys_quiet(gx: &crate::graph::Gx<'_, '_>, op: &Op<'_>) -> bool {
    !matches!(op, Op::Join(JoinDesc::Probe { keys, .. }) if !keys.iter().all(|n| gx.fx(n).quiet()))
}

/// D1906: a call in `n` the side class does not read (it reads a FuncExpr's and an OpExpr's function; D1871's kinds): a
/// user operator or function a ScalarArrayOpExpr, DistinctExpr, NullIfExpr or RowCompareExpr runs, a user type's I/O
/// (CoerceViaIO), a domain's CHECK, an identity column's nextval, a SubPlan node, a row-reading xml builtin (its own
/// query runs what the relations it names call: a view's nextval), or a node
/// the walk cannot see. A called function is user code by the walk's one rule (effects/user_fn.rs `user_call`); an operator
/// or a type is a user's past FIRST_NORMAL_OID.
pub(crate) fn hidden(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    crate::effects::user_fn::walk_by(gx.facts, n, &crate::effects::user_fn::Reading::hidden(), &|t| crate::effects::type_free(gx.catalog, t, 0))
}

/// D1906 (the kept-build road, lifetime/hashjoin.rs `kept_road` and `rerun_order`): build `b`'s side calls a user or
/// VOLATILE function, which PG runs only at a call that makes the build: visibly (the side class's UserFn,
/// build_class.rs `side_by`; a side the class does not see into, probe.rs `opaque`) or, with `hidden`, through code the
/// side class does not read (probe_kept.rs `hidden`: a value of a user type, a row-reading xml builtin).
pub(crate) fn build_calls(gx: &Gx<'_, '_>, b: usize, hidden: bool) -> bool {
    use crate::ops::hashjoin::{build_class::side_by, probe, probe_kept};
    side_by(gx, b, usize::MAX, false) == Side::UserFn || probe::opaque(gx, b) || (hidden && probe_kept::hidden(gx, b))
}

/// D3566 (effects.rs `hides_calls`): `hidden` with SubPlan nodes left to the caller (user_fn `Reading::hidden_parts`).
pub(crate) fn hidden_parts(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    crate::effects::user_fn::walk_by(gx.facts, n, &crate::effects::user_fn::Reading::hidden_parts(), &|t| crate::effects::type_free(gx.catalog, t, 0))
}

/// build_class.rs `own_side_by` under PG's quiet reading: set function `f` raises nothing PG 18.6 would not (its call
/// quiet, no value of a user type in it or its column types, its arguments quiet by `rf`).
pub(crate) fn srf_quiet(gx: &Gx<'_, '_>, f: &gravity_decompose::ops::shared::srf::SrfFn<'_>, rf: &dyn Fn(Node<'_>) -> bool) -> bool {
    gravity_decompose::effects::quiet_call(gx.facts, f.funcid) && !user_typed(gx, f.node) && f.coltypes.iter().all(|t| t < FIRST_NORMAL_OID) && f.args.iter().all(rf)
}

/// probe.rs `pulls` (V4R-512, D1670): a CTE InitPlan whose facts show at PG's first outer fetch: it may raise, or it makes
/// a VOLATILE call.
pub(crate) fn init_loud(raise_free: bool, volatility: Volatility) -> bool {
    !raise_free || volatility == Volatility::Volatile
}

/// D3273 (ops/hashjoin/build_class.rs `defers`): pipeline `b` is a settled union's emit (rerun/union_arms.rs `union_of`)
/// that no rerun set runs, one of whose arms, with what feeds it, may raise or call (`side`).
pub(crate) fn loud_arms(gx: &Gx<'_, '_>, b: usize) -> bool {
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(b as u16)));
    !rerun && gx.union_feeds(b).iter().any(|&f| side(gx, f) != Side::Safe)
}

/// Pipeline `k`'s own ops over its source, without what it consumes: its filters, gates and projections and its inner
/// probes' keys and quals (rtfilter.rs `raise_free`); a scan or a breaker source is quiet, any other op or source may raise.
/// D762: a source or a set that calls a user function (a VALUES row, a table function's document, PASSING values or columns,
/// an index or bitmap key, a set function's body or arguments) calls it where PG's node does (M5-ENTRY-SPI §4.1 R2), so its
/// side is a user function's, as a filter's is: expr.volatile's order or the probe's named refusal, never a deferral.
/// D1005: so is a VOLATILE builtin's (nextval, random): PG never makes the calls of a side it never runs,
/// and op 14's deferral ran them (an outer-first build under an empty outer called nextval once per build row).
pub(crate) fn own_side(gx: &Gx<'_, '_>, k: usize) -> Side {
    let ops = &gx.pipelines()[k].ops;
    let upto = ops.len() - matches!(ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) as usize; // a build's keys: its probe's
    own_side_by(gx, k, &|n| side_calls(gx, n), None, upto)
}

/// `own_side` over the first `upto` ops (a build's keys at its trailing op), with `uf` the calls that make a side a user
/// function's. `by`: `side_by`'s reading (D1902), the PARAM_EXEC ids it names read as bound words (rtfilter.rs
/// `raise_free_by`), and with its flag what PG 18.6 runs without raising read quiet, expression by expression (effects.rs
/// `raise_free_each`; an expression holding a value of a user type reads as trunk): a Const (Result)
/// source, an initdb btree index (builtin order procs) of quiet keys and recheck (`quiet_key`), a set op of calls the
/// effects home reads quiet (`effects::quiet_call`) on quiet args, none of a user type; a pipeline feeding a group whose
/// keys, aggregates or FILTERs hold a value of a user type (its comparisons and folds run that type's functions) raises.
pub(crate) fn own_side_by(gx: &Gx<'_, '_>, k: usize, uf: &dyn Fn(Node<'_>) -> bool, by: Option<(&dyn Fn(i32) -> bool, bool)>, upto: usize) -> Side {
    own_side_nests(gx, k, uf, by, upto, false)
}

/// `own_side_by`; `nests`: a nest step `effects::nest_step_quiet` passes (by `by`'s expression reading) keeps the side, and
/// so does a Limit of settled bounds (`frame::iface::limit_settled`).
pub(crate) fn own_side_nests(gx: &Gx<'_, '_>, k: usize, uf: &dyn Fn(Node<'_>) -> bool, by: Option<(&dyn Fn(i32) -> bool, bool)>, upto: usize, nests: bool) -> Side {
    own_side_with(gx, k, uf, by, upto, nests, false)
}

/// `own_side_nests`; `merges` (the tracer's stamp only, `stamp_side`): a MergeAppend's ordered feed and its merge read as
/// quiet where they compare nothing that may raise.
fn own_side_with(gx: &Gx<'_, '_>, k: usize, uf: &dyn Fn(Node<'_>) -> bool, by: Option<(&dyn Fn(i32) -> bool, bool)>, upto: usize, nests: bool, merges: bool) -> Side {
    use crate::effects::quiet_key;
    let p = &gx.pipelines()[k];
    let rf = |n: Node<'_>| by.map_or_else(|| gx.fx(n).quiet(), |(q, a)| if a { gx.fx(n).quiet_each(q) } else { gx.fx(n).quiet_by(q, false) });
    let of = |n: Node<'_>| if uf(n) { Side::UserFn } else if rf(n) { Side::Safe } else { Side::Raises };
    let ops = &p.ops[..upto.min(p.ops.len())];
    let calls = match &p.source {
        Source::Values { lists, .. } => lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(|n| uf(n)),
        Source::TableFunc { func: f, .. } => {
            let cols = [&f.colexprs, &f.coldefexprs, &f.colvalexprs].into_iter().flat_map(|l| l.iter().flatten());
            f.docexpr.into_iter().chain(f.rowexpr).chain(f.passingvalexprs.iter()).chain(cols).any(|n| uf(n))
        }
        Source::Index { keys, recheck, .. } => keys.iter().map(|k| k.value).chain(recheck.iter()).any(|n| uf(n)),
        Source::Bitmap { recheck, .. } => recheck.iter().any(|n| uf(n)),
        _ => false,
    };
    // D2063's facts (`initdb_index`): a user table's index whose opclasses and key operators are initdb ones reads as a
    // system index does; a bitmap is whole before its first row (PG's MultiExecBitmapIndexScan), every index a btree one
    let quiet = match (&p.source, by) {
        (Source::Range { .. } | Source::Breaker { .. }, _) | (Source::Const, Some((_, true))) => true,
        // with#170 / #171: a VALUES list of expressions that raise and call nothing (a CTE's constant rows). A list of no
        // rows (no plan makes one: the grammar needs a row) keeps the old reading, the unit tests' stand-in that may raise
        (Source::Values { lists, .. }, _) => !lists.is_nil() && lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).all(|n| !uf(n) && rf(n)),
        (Source::Index { keys, recheck, indexid, iam: IndexAm::Btree, .. }, Some((q, true))) => initdb_index(gx, *indexid, keys) && keys.iter().all(|x| quiet_key(gx, x, q)) && recheck.iter().all(rf),
        (Source::Bitmap { plan, recheck, .. }, Some((q, true))) => bitmap_all(plan, &|i, k, am| am == IndexAm::Btree && initdb_index(gx, i, k) && k.iter().all(|x| quiet_key(gx, x, q))) && recheck.iter().all(rf),
        _ => false,
    };
    let group = by.is_some_and(|(_, a)| a) && gx.group_at(k).is_some_and(|g| g.keys.iter().any(|x| user_typed(gx, x.expr.node())) || g.aggs.iter().any(|a| user_typed(gx, a.node) || a.filter.is_some_and(|f| user_typed(gx, f))));
    let base = if calls { Side::UserFn } else if quiet && !group { Side::Safe } else { Side::Raises };
    ops.iter().fold(base, |w, op| match op {
        Op::Filter(n) | Op::Gate(n) => w.max(of(*n)),
        Op::Project(l) => l.iter().fold(w, |w, n| w.max(of(n))),
        Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).fold(w, |w, n| w.max(of(n))),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().fold(w, |w, n| w.max(of(n))),
        Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Feed { ordered: false, .. } | gravity_decompose::ops::shared::append::AppendDesc::Union { prune: None, .. }) => w, // a settled union's row, moved as it is (nest/union_join_arms.rs)
        // D3373 (D1902): a recursive UNION ALL's iteration moves its generations' rows as they are, the seed's and the
        // term's evaluations their own pipelines' (each read here as a feeder); in a CTE's pulled set (`in_pulled_set`) its
        // rows are made only as its readers pull, never ahead of PostgreSQL's CTE Scan, so it evaluates nothing of its own.
        // A UNION's dedup compares rows: it keeps the default reading
        Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Iter(it)) if it.dedup.is_empty() && crate::ops::hashjoin::build_class::in_pulled_set(gx, k) => w,
        Op::Srf(d) if d.functions.iter().any(|f| uf(f.node)) => w.max(Side::UserFn),
        Op::Srf(d) if by.is_some_and(|(_, a)| a) && d.functions.iter().all(|f| srf_quiet(gx, f, &rf)) => w,
        // errorhold (D1904): under the PG-quiet reading a sort or a merge join compares only through
        // PG's own btree support, so one of quiet keys raises nothing (effects.rs `sort_quiet`, `merge_quiet`)
        Op::Sort(sd) if by.is_some_and(|(_, a)| a) && crate::effects::sort_quiet(gx, sd, &rf) => w,
        Op::Join(j @ JoinDesc::Merge { .. }) if by.is_some_and(|(_, a)| a) && crate::effects::merge_quiet(j, &rf) => w,
        // D3178 (`between_safe`): a nest whose inner raises nothing and calls nothing VOLATILE (F5), its qual and parameters
        // raise-free, raises and acts nowhere
        Op::Join(j @ JoinDesc::Nest { .. }) if nests && !crate::effects::nest_calls(gx, j) && crate::effects::nest_step_quiet(op, &rf, false) => w,
        // D3374 (`between_safe`): a Limit of settled bounds (none, or constants of at least 0) evaluates nothing and raises
        // nothing (a nest's uncorrelated inner spooled under its LIMIT, rerun_d2194 B4)
        op if nests && crate::frame::iface::limit_settled(op) => w,
        // D2107: a nest's embedded inner runs per outer row where the nest does: a VOLATILE call in it (nextval) makes the
        // side a user function's, as a filter's does (expr.volatile's order, never op 14's eager deferral)
        Op::Join(j @ JoinDesc::Nest { .. }) => w.max(if crate::effects::nest_calls(gx, j) { Side::UserFn } else { Side::Raises }),
        // D4137 (D2107's reading for a SubPlan): a SubPlan step whose call acts (`SubPlanDesc::answer`: a VOLATILE call
        // or user code in its plan, test or args, a scalar sub-SELECT's nextval) runs where its expression is evaluated, so
        // the side calls as a filter's does; read as a raise, op 14's eager build made its calls before PG's first outer
        // row. A data-modifying CTE its plan reads is its store (PG runs that CTE to its end either way): it raises
        Op::SubPlan(d) if d.answer().effects.acts => w.max(Side::UserFn),
        // D2731: a Material's spool write moves the rows its pipeline made (PG's tuplestore grows or spills, it evaluates
        // nothing): the side is its source's and earlier ops'
        Op::Join(JoinDesc::Spool) => w,
        // C2 (b), the tracer's stamp only (`stamp_side`): a MergeAppend child's run moves into the merge's input as it is
        // (an ordered feed evaluates nothing), and the merge compares only through PG's own btree support when every key
        // passes `sort_quiet`'s key test (`effects::sort_keys_quiet`); a pruned merge evaluates its pruning steps
        Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Feed { ordered: true, .. }) if merges => w,
        Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Merge { keys, prune: None, .. }) if merges && by.is_some_and(|(_, a)| a) && crate::effects::sort_keys_quiet(gx, keys) => w,
        // D3897: a RIGHT / FULL (or RIGHT SEMI / ANTI) drain hands on the build's rows by their match bits and evaluates
        // nothing (its join's keys and qual are the probe's, a filter after it its own op): the side is its source's
        Op::Join(JoinDesc::Drain { .. }) => w,
        // with#170 / #171: a window that raises on no input (effects.rs `window_quiet`) evaluates nothing that shows
        Op::Window(d) if crate::effects::window_quiet(d, &|n| !uf(n) && rf(n)) => w,
        // D4145: a window that calls (`window_calls`: a VOLATILE argument or projection, an acting function, run condition or
        // offset, a user partition or order operator) makes the side a user function's, as a filter's call does
        op @ Op::Window(_) if crate::effects::window_calls(op, uf) => w.max(Side::UserFn),
        _ => w.max(if matches!(op, Op::Lock(_)) { Side::UserFn } else { Side::Raises }),
    })
}

/// Build pipeline `k`'s side: its own ops and every pipeline feeding it (inner probes' builds, a breaker source), read
/// with `side_calls` (`side_with`).
pub(crate) fn side(gx: &Gx<'_, '_>, k: usize) -> Side {
    side_with(gx, k, &|n| side_calls(gx, n))
}

/// D3816 (`Px::run_ahead`'s acting rule): `side` with the calls that act alone making it a user function's
/// (`side_acts`); a pinned non-acting builtin's call may still make it raise.
pub(crate) fn acting_side(gx: &Gx<'_, '_>, k: usize) -> Side {
    side_with(gx, k, &|n| side_acts(gx, n))
}

/// D3336 (A2 (iii)): `acting_side` is a user function's (`Side::UserFn`): `Px::run_ahead`'s acting rule over pipeline `k`
/// and what feeds it, which a strategy reads before it chooses a form that rule must then license (ops/ctescan/wholefill.rs
/// `licensable`), so it names no other family's side class (lint RA).
pub(crate) fn acting_user_fn(gx: &Gx<'_, '_>, k: usize) -> bool {
    acting_side(gx, k) == Side::UserFn
}

/// Build pipeline `k`'s side, the side class's one fold of `own_side_by` over its own ops and every pipeline feeding it
/// (inner probes' builds, a breaker source), `uf` the calls that make a side a user function's.
pub(crate) fn side_with(gx: &Gx<'_, '_>, k: usize, uf: &dyn Fn(Node<'_>) -> bool) -> Side {
    let own = |x: usize| {
        // a build's keys are its probe's (`own_side`)
        let ops = &gx.pipelines()[x].ops;
        own_side_by(gx, x, uf, None, ops.len() - matches!(ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) as usize)
    };
    let p = &gx.pipelines()[k];
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    feed(gx, k, p.desc.consumes.iter().copied().chain(src)).into_iter().fold(own(k), |w, x| w.max(own(x)))
}

/// D4191 (construct C2 (b): the run-ahead tracer's `quiet` stamp only, lowerv4 frame/program.rs `gate_stamp`): the groups
/// pipeline `k` and every pipeline feeding it fold into, read by their keys, aggregate arguments and FILTERs as
/// effects.rs `walk_reversible` reads a group's. `side` reads ops only, so a hashed aggregate's count(f(v)) or a hashed
/// DISTINCT's f(v) key, which the group evaluates per row, read Safe there.
pub(crate) fn groups_side(gx: &Gx<'_, '_>, k: usize) -> Side {
    let p = &gx.pipelines()[k];
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let of = |n: Node<'_>| if side_calls(gx, n) || user_typed(gx, n) { Side::UserFn } else if gx.fx(n).quiet() { Side::Safe } else { Side::Raises };
    let args = |d: &gravity_decompose::ops::shared::agg::AggDecl<'_>| {
        let r = d.node.as_aggref();
        let entries = r.into_iter().flat_map(|r| r.aggdirectargs.iter().chain(r.args.iter().map(|t| t.as_target_entry().map_or(t, |t| t.expr))));
        entries.chain(d.filter).fold(Side::Safe, |w, n| w.max(of(n)))
    };
    let group = |x: usize| gx.group_at(x).map_or(Side::Safe, |g| g.aggs.iter().fold(g.keys.iter().fold(Side::Safe, |w, c| w.max(of(c.expr.node()))), |w, d| w.max(args(d))));
    feed(gx, k, p.desc.consumes.iter().copied().chain(src)).into_iter().fold(group(k), |w, x| w.max(group(x)))
}

/// D1902 (lifetime/hashjoin.rs `rerun_order`): pipeline `k`'s side through its first `upto` ops (a build's keys too at its
/// whole length; D1904's `key`: op `upto`'s outer hash keys too, not its qual) and its feeders' (`feed` of all it reads but
/// the builds of its probes from op `upto` on), a PARAM_EXEC a nest or a per-row SubPlan binds read as a bound word, and
/// what PG 18.6 runs without raising as quiet, expression by expression (W4b-RowX: `own_side_by`'s `by`;
/// an expression holding a value of a user type reads as trunk, effects.rs `raise_free_each`) unless the statement
/// holds code no expression shows (`quiet_statement`).
pub(crate) fn side_by(gx: &Gx<'_, '_>, k: usize, upto: usize, key: bool) -> Side {
    side_by_quiet(gx, k, upto, key, quiet_statement(gx))
}

/// D3374 (probe_calls.rs `rescans`, D1904's later-call road): `side_by` with the statement read by
/// `quiet_statement_at(q, j)`: probe op `j` of pipeline `q`'s hash clauses count by their operators, their keys being
/// the sides that road reads by name, so a user function in that probe's outer key keeps the PG-quiet reading of the
/// rest (an index outer over initdb opclasses reads as quiet, D2063's facts, as it does in a statement with no such key).
pub(crate) fn side_by_at(gx: &Gx<'_, '_>, k: usize, upto: usize, key: bool, q: usize, j: usize) -> Side {
    side_by_quiet(gx, k, upto, key, quiet_statement_at(gx, q, j))
}

/// C2 (b), the run-ahead tracer's quiet stamp (lowerv4 frame/program.rs `gate_stamp`): `side_by`'s whole-pipeline reading,
/// where a MergeAppend's ordered feed and its merge over keys that compare through PG's own btree support also read as
/// quiet (`own_side_with`'s `merges`): PostgreSQL's MergeAppend raises there no more than its Sorts do. The tracer's stamp
/// only; serving reads `side_by`.
pub(crate) fn stamp_side(gx: &Gx<'_, '_>, k: usize) -> Side {
    side_by_with(gx, k, usize::MAX, false, quiet_statement(gx), true)
}

fn side_by_quiet(gx: &Gx<'_, '_>, k: usize, upto: usize, key: bool, quiet: bool) -> Side {
    side_by_with(gx, k, upto, key, quiet, false)
}

fn side_by_with(gx: &Gx<'_, '_>, k: usize, upto: usize, key: bool, quiet: bool, merges: bool) -> Side {
    let (p, bound, uf) = (&gx.pipelines()[k], |id: i32| gx.graph.params.iter().any(|x| x.0 == id), |n: Node<'_>| side_calls(gx, n));
    let rf = |n: Node<'_>| if quiet { gx.fx(n).quiet_each(&bound) } else { gx.fx(n).quiet_by(&bound, false) };
    let of = |n: Node<'_>| if uf(n) { Side::UserFn } else if rf(n) { Side::Safe } else { Side::Raises };
    let hk = match p.ops.get(upto) { Some(Op::Join(JoinDesc::Probe { keys, .. })) if key => keys.iter().map(of).max(), _ => None };
    let f = feed(gx, k, p.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = p.source { Some(slot) } else { None }).filter(|s| !p.ops.iter().skip(upto).any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == *s))));
    f.into_iter().fold(own_side_with(gx, k, &uf, Some((&bound, quiet)), upto, false, merges).max(hk.unwrap_or(Side::Safe)), |w, x| w.max(own_side_with(gx, x, &uf, Some((&bound, quiet)), usize::MAX, false, merges)))
}

/// D3788: `own_side` with a trailing build's keys read too (`held_before`'s reading): what pipeline `k` runs itself.
pub(crate) fn keyed_side(gx: &Gx<'_, '_>, k: usize) -> Side {
    own_side_by(gx, k, &|n| side_calls(gx, n), None, usize::MAX)
}
