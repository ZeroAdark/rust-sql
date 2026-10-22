//! The effects home's merge file: the raise-free and quiet readings MergeAppend's admission asks
//! (ops/append/merge_admit.rs: D449, D1417, V4R-465; D2884: the window readings `tie_shown` asks) and merge.join's inner and outer walks ask (ops/mergejoin
//! markable.rs, lazy_outer.rs, fed_first.rs, nest_inner.rs: D1236 / D3634; `may_act`, `user_sort` and `raise_only` live here).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::sort::SortKey;
use gravity_decompose::{IndexKey, Op, PipelineNode, Source};
use mcx::{GrowIn, PgVec};
use types_nodes::list::NodeList;
use types_nodes::Node;

use crate::graph::Gx;
use crate::ops::append::merge_admit::{arm_cols, feed_at, full_sort, Merge};

/// D449's raise-free expression: the raise-free walk's shared leaf set (raise_free.rs `leaf` and `own_call`: a column, a
/// constant, an extern parameter (a PARAM_EXEC may run an InitPlan or SubPlan that raises), CASE, COALESCE, ROW, GREATEST /
/// LEAST over a type whose compare is pure, a constant pattern match that cannot raise, ..) with a builtin call the effects
/// home calls quiet, a quiet comparison = ANY / ALL, over raise-free arguments. An aggregate is not a row's.
pub(crate) fn pure(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    pure_in(gx, n, false, &[])
}

/// `pure` with the PARAM_EXEC ids `vals` read as values: a nest loop's own params inside its inner (merge/markable.rs).
pub(crate) fn pure_given(gx: &Gx<'_, '_>, n: Node<'_>, vals: &[i32]) -> bool {
    pure_in(gx, n, false, vals)
}

/// `pure`; with `counts`, the one raise-free reading's fold form (D1417, OID-29: a raise-free aggregate is a leaf, its
/// arguments and FILTER kids); a PARAM_EXEC in `vals` is a value. The effects home's one walk (effects/raise_free.rs).
pub(crate) fn pure_in(gx: &Gx<'_, '_>, n: Node<'_>, counts: bool, vals: &[i32]) -> bool {
    gx.fx(n).quiet_in(&crate::effects::raise_free::Reading::merge(&|id| vals.contains(&id), counts))
}

/// D449's rule over a pipeline's ops: a filter or a projection entry that is not raise-free can raise at a row; so can a
/// hash join key whose hash PostgreSQL raises on (D2849, effects `raising_key`). D3955: so can a nested loop at each outer
/// row, where its inner plan (F5 `raise_free`, the nest's facts, and its inner's ops read by this rule), its join qual or
/// a value it binds may raise, and a SubPlan step where its plan, test or arguments may (its desc's F5 facts).
pub(crate) fn ops_can_raise(gx: &Gx<'_, '_>, ops: &[Op<'_>]) -> bool {
    crate::stack_check(); // a nest's inner is read by this rule too
    ops.iter().any(|op| match op {
        Op::Project(l) => !l.iter().all(|n| pure(gx, n)),
        Op::Filter(q) => !pure(gx, *q),
        Op::Join(JoinDesc::Build { keys, .. } | JoinDesc::Probe { keys, .. }) => keys.iter().any(|n| crate::effects::raising_key(gx, n)),
        Op::Join(JoinDesc::Nest { params, qual, facts, inner, .. }) => {
            !facts.raise_free
                || ops_can_raise(gx, &inner.ops)
                || !qual.iter().all(|n| pure(gx, n))
                || !params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| pure(gx, p.paramval)))
        }
        Op::SubPlan(d) => !d.raise_free,
        _ => false,
    })
}

/// V4R-465: under a reader that pulls each arm of MergeAppend `m` once (merge_admit.rs `pulls_once`), the breaker form reads
/// the same rows in the same order: each serial arm capped at one row (runs/mergeappend.rs `Cap`), a Sort arm's input
/// pipeline run whole before its arm. So an arm may raise where it reads rows one at a time: a btree scan keyed by null
/// tests alone or a full sort's run, its filters raise-free. Keys are builtin scalar orders (a user comparator's calls
/// would count otherwise).
pub(crate) fn first_row(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    let Some(cols) = m.arms.last().and_then(|&a| arm_cols(&ps[a])) else { return false };
    let arm = |a: usize| {
        let p = &ps[a];
        let f = feed_at(p);
        let quiet = !ops_can_raise(gx, &p.ops[..f]);
        let steps = p.ops[..f].iter().all(|op| matches!(op, Op::Project(_)) || matches!(op, Op::Filter(q) if pure(gx, *q)));
        let serial = matches!(p.desc.parallel, gravity_program::Parallel::Ineligible { .. });
        let src = match &p.source {
            // a btree returns no lossy match: its indexqualorig is never rechecked
            Source::Index { am: gravity_decompose::Am::Heap, iam: gravity_decompose::IndexAm::Btree, keys, .. } => {
                keys.iter().all(|k| matches!(k.form, gravity_decompose::graph::KeyForm::NotNull | gravity_decompose::graph::KeyForm::Null))
            }
            Source::Breaker { slot } => gx.life_of(a, *slot).is_some_and(|l| full_sort(gx, l.0 as usize, *slot, a)),
            _ => quiet,
        };
        src && (quiet || serial && steps)
    };
    let keys = m.keys.iter().all(|k| builtin_key(gx, k, cols));
    keys && m.arms.iter().all(|&a| arm(a))
}

/// V4R-465, PM2: a key its type's default btree family orders (neither `own_family` nor `no_proc`), over a type whose
/// compare neither acts nor raises (`cmp_pure`).
pub(crate) fn builtin_key(gx: &Gx<'_, '_>, k: &SortKey, cols: &NodeList<'_>) -> bool {
    let te = (k.resno as usize).checked_sub(1).and_then(|i| cols.iter().nth(i)).and_then(|n| n.as_target_entry());
    let typ = te.map(|te| gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr)));
    !k.own_family && k.no_proc.is_none() && typ.is_some_and(|t| crate::effects::raise_free::cmp_pure(gx, t))
}

/// Pipeline `p`'s source and its ops before `upto` raise on no row and call no user or volatile function; the slots it
/// reads go to `feeds`. D1236 (MERGE-3b): a join is quiet when its keys, clauses and qual are (a hash probe's build, a
/// merge's inner, a nest loop's inner and a memo's rows are read like any fed slot; a nest loop's inner pipeline is
/// checked here, its parameters too); a Result, VALUES or bitmap source when its rows (a bitmap's recheck) are.
pub(crate) fn pipe_quiet(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, upto: usize, feeds: &mut PgVec<'_, u8>) -> bool {
    pipe_given(gx, p, upto, feeds, None)
}

/// `pipe_quiet`; inside a nest loop's inner, `vals` holds its and the enclosing nests' NestLoopParam ids (W8-D1236-CLASSA):
/// PG sets them from the outer row before each rescan, so they read as values (an InitPlan's or SubPlan's param still acts),
/// and there only builtin code may run where no expression shows it: a system index through builtin operators (its
/// support functions are the catalog's own), a heap scan, VALUES, filters, projections, gates and nest loops.
fn pipe_given(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, upto: usize, feeds: &mut PgVec<'_, u8>, vals: Option<&[i32]>) -> bool {
    crate::stack_check(); // D3832
    // or the effects home's raise-free reading (effects.rs `raise_free_by`, the nest's parameters read as values): it takes
    // a builtin aggregate whose transitions and final raise on no input, as a group's emit projects them (CNM-26), so a
    // group that folds ahead of the merge's pull differs from PG's GroupAggregate in no row it hands on; no user or
    // volatile call either way
    let q = |n: Node<'_>| {
        let given = vals.unwrap_or(&[]);
        pure_given(gx, n, given) || crate::effects::raise_free_by(gx, n, &|p| given.contains(&p), false) && !crate::effects::user_fn(gx, n) && !crate::effects::volatile(gx, n)
    };
    let all = |l: &NodeList<'_>| l.iter().all(q);
    let sys = |oid: u32| vals.is_none() || oid < FIRST_NORMAL_OID;
    let quiet_src = match &p.source {
        Source::Range { .. } | Source::Const => true,
        Source::Breaker { .. } => vals.is_none(),
        // D3807 (River's JobSchedule after table bloat: the conflicts a Nested Loop of jobs_with_rownum's rows and the unique
        // index's scan keyed by them): or, in a nest's inner, a user table's index through builtin operators whose
        // operator class and key operators neither act nor raise (`quiet_ix`): the AM calls only those
        Source::Index { keys, recheck, indexid, .. } => (sys(*indexid) || quiet_ix(gx, *indexid, keys)) && builtin_ix(gx, *indexid, keys) && keys.iter().all(|x| sys(x.opno) && q(x.value)) && all(recheck),
        Source::Values { lists, .. } => lists.iter().all(|row| row.as_list().is_some_and(|r| all(r))),
        Source::Bitmap { plan, recheck, .. } => vals.is_none() && crate::effects::bitmap_all(plan, &|i, k, _| builtin_ix(gx, i, k)) && all(recheck),
        _ => false,
    };
    let quiet_ops = p.ops.iter().take(upto).all(|op| match op {
        // a one-time gate is an implicit-AND list (a Result's resconstantqual)
        Op::Filter(n) | Op::Gate(n) => n.as_list().map_or_else(|| q(*n), all),
        Op::Project(l) => all(l),
        Op::Join(JoinDesc::Nest { params, inner, qual, .. }) => {
            let mut ids = mcx::to_vec_in(vals.unwrap_or(&[]), gx.mcx);
            let given = params.iter().all(|n| n.as_nest_loop_param().is_some_and(|x| q(x.paramval) && { ids.push_in(x.paramno); true }));
            given && all(qual) && pipe_given(gx, inner, usize::MAX, feeds, (vals.is_some() || !params.is_nil()).then_some(&ids[..]))
        }
        _ if vals.is_some() => false,
        // D3799 (River's jobs_with_rownum: row_number() over each unique key's partition): a window whose functions raise
        // on no input (row_number, rank, dense_rank: types_core quiet `window_quiet`) with no FILTER, its partition and
        // order equalities raising on none (`eq_raise_free`), no run condition and no frame offset, nothing VOLATILE: PG's
        // WindowAgg compares each row's keys with its predecessor's and counts
        Op::Window(w) => {
            let rank = |f: &gravity_decompose::ops::window::WinFn<'_>| types_core::quiet::window_quiet(f.func.winfnoid, f.func.args.len()) && f.func.aggfilter.is_none();
            let offsets = w.plan.startOffset.is_none() && w.plan.endOffset.is_none();
            !w.volatile_args && !w.volatile_proj && w.run_condition.is_nil() && offsets && w.functions.iter().all(rank) && w.partition.iter().chain(w.order.iter()).all(|key| crate::effects::eq_raise_free(gx, key.opno))
        }
        // D2283: PG's recompute_limits raises 2201W / 2201X on a negative LIMIT / OFFSET at the Limit's first call, which a
        // merge's inner makes only when an outer row needs it: a bound a run can make negative is not quiet (a top-N sort's
        // own bound alike; the effects home's `bounds_may_raise`)
        Op::Sort(_) | Op::Limit { .. } => !crate::effects::visible::bounds_may_raise(op),
        // a DISTINCT ON's first of each run (V4R-147): PG's Unique calls each key's equality on every row it reads, quiet
        // when that raises on no input (effects; run.pergroup compares the type's own image, raising on none)
        Op::FirstOfRun(t) => t.keys.iter().all(|key| crate::effects::eq_raise_free(gx, key.opno)),
        Op::Join(JoinDesc::Spool | JoinDesc::Drain { .. }) => true,
        Op::Join(JoinDesc::Build { keys, .. } | JoinDesc::Memo { keys, .. }) => all(keys),
        Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => all(clauses) && all(keys) && all(qual),
        Op::Join(JoinDesc::Merge { clauses, qual, .. }) => all(clauses) && all(qual),
        // a sorted or hashed SetOp (INTERSECT / EXCEPT) compares its keys and counts each group's rows: quiet when every key's
        // equality raises on no input (`eq_raise_free`) and its type's compare (a hashed one's hash too) neither acts nor
        // raises; its two inputs are read like any fed slot
        Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Set { hashed, keys, tlist, .. }) => all(tlist) && keys.iter().all(|key| {
            let e = if *hashed { gravity_decompose::effects::of_type(gx.facts, key.typid) } else { gravity_decompose::effects::of_compare(gx.facts, key.typid) };
            crate::effects::eq_raise_free(gx, key.opno) && !e.acts && !e.may_raise
        }),
        // an Append's routing ops move rows; a run-time prune evaluates expressions, so it is not quiet
        Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { prune: None, .. } | AppendDesc::Union { prune: None, .. }) => true,
        // D1289 (material, foreign_key#1393: the RI check's Merge Right Anti Join over an Append of MergeAppends): a merge
        // compares its keys, each here an order of its type's default family whose compare neither acts nor raises
        // (effects' `of_compare`, `builtin_key`'s test); its children are read as any fed slot is
        Op::Append(AppendDesc::Merge { prune: None, keys, .. }) => keys.iter().all(|key| {
            let cmp = gx.facts.key(gravity_decompose::catalog::KeyReq::sort(key.opno)).filter(|o| o.default_family).map(|o| gravity_decompose::effects::of_compare(gx.facts, o.opcintype));
            !key.own_family && key.no_proc.is_none() && cmp.is_some_and(|e| !e.acts && !e.may_raise)
        }),
        _ => false,
    });
    feeds.extend_in(p.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = p.source { Some(slot) } else { None }));
    quiet_src && quiet_ops
}

/// D2142: index `i` with key operators `k` runs no user code: an index whose operator class or key operator is a user's
/// calls it at each comparison (a user btree opclass's FUNCTION 1), which PG's merge never runs when no outer row reaches
/// the inner (effects' `of_index`: unknown, so acting, for any opclass or operator that is not an initdb one; a system
/// index, D2063's `initdb_index`, is the catalog's own). D3604: an `IS [NOT] NULL` key (a partial index's `unique_key IS
/// NOT NULL`) names no operator: the AM tests the key's null flag, calling nothing, so it adds no call.
fn builtin_ix(gx: &Gx<'_, '_>, i: u32, k: &[IndexKey<'_>]) -> bool {
    let ops = k.iter().filter(|x| !matches!(x.form, gravity_decompose::graph::KeyForm::NotNull | gravity_decompose::graph::KeyForm::Null)).map(|x| x.opno);
    crate::effects::initdb_index(gx, i, k) || !gravity_decompose::effects::of_index(gx.facts, i, ops).acts
}

/// D3807: index `i` with keys `k` calls only builtin code that raises on no input: every key column's operator class an
/// initdb one and each key operator an initdb one (effects' `of_index`) whose call neither acts nor may raise; an `IS
/// [NOT] NULL` key names no operator (`builtin_ix`).
fn quiet_ix(gx: &Gx<'_, '_>, i: u32, k: &[IndexKey<'_>]) -> bool {
    let ops = k.iter().filter(|x| !matches!(x.form, gravity_decompose::graph::KeyForm::NotNull | gravity_decompose::graph::KeyForm::Null)).map(|x| x.opno);
    let e = gravity_decompose::effects::of_index(gx.facts, i, ops);
    !e.acts && !e.may_raise
}

/// Pipeline `c`'s source and its ops before `upto` are quiet (`pipe_quiet`, the slots it reads not kept): merge.join's
/// lazy inner and its outer feeders (ops/mergejoin/lazy_outer.rs).
pub(crate) fn quiet(gx: &Gx<'_, '_>, c: usize, upto: usize) -> bool {
    pipe_quiet(gx, &gx.pipelines()[c], upto, &mut PgVec::new_in(gx.mcx))
}

/// D2884: WindowAgg `d`, filled from a Merge Append's rows, reads no column but those `safe` passes (its target list, its
/// functions' arguments, its qual: the merge-key columns whose equal values print alike), so it hands on the same rows
/// whichever member of a run of keys equal across subplans came first (merge_admit.rs `tie_shown`).
pub(crate) fn window_reads_only(d: &gravity_decompose::ops::window::WindowDesc<'_>, safe: &dyn Fn(i16) -> bool) -> bool {
    let mut ok = true;
    for n in d.tlist.iter().chain(d.plan.plan.qual.iter()) {
        gravity_decompose::expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0) {
                ok &= safe(v.varattno);
            }
            ok
        });
    }
    ok
}

/// D2884: every function of WindowAgg `d` reads no order among its peers, so no member of a run of keys equal across a
/// Merge Append's subplans shows through it: rank, dense_rank, percent_rank, cume_dist, or an aggregate over a RANGE /
/// GROUPS frame (whole peer groups) that is a count (`types_core::quiet::PEER_ORDER_FREE`) or whose arguments `alike`
/// passes (equal values print alike).
pub(crate) fn window_peers_free(d: &gravity_decompose::ops::window::WindowDesc<'_>, alike: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::ops::shared::window::FrameMode;
    use gravity_decompose::ops::window::WinFnKind;
    d.functions.iter().all(|f| match f.kind {
        WinFnKind::Rank | WinFnKind::DenseRank | WinFnKind::PercentRank | WinFnKind::CumeDist => true,
        WinFnKind::Aggregate => !matches!(d.frame.mode, FrameMode::Rows) && (types_core::quiet::peer_order_free(f.func.winfnoid) || f.func.args.iter().all(|a| alike(a))),
        _ => false,
    })
}

/// D1236: whether the inner's rows may act where PG would never produce them: some pipeline that fills the inner run
/// (`q` and all it consumes) can raise or calls a user or volatile function. PG runs the inner only once a matchable outer
/// tuple needs it, and a spooled inner only as far as the join reads; v4 runs it whole, first. D1289: the outer, pipeline
/// `q`'s ops before `upto` (the merge's), where a RIGHT join stops fetching at its first NULLS LAST NULL key.
pub(crate) fn may_act(gx: &Gx<'_, '_>, q: usize, upto: usize) -> bool {
    may_act_but(gx, q, upto, None)
}

/// `may_act` with pipeline `q`'s slot `skip` left out (MERGE R3 (ii): the merge's outer side, its own inner not counted).
pub(crate) fn may_act_but(gx: &Gx<'_, '_>, q: usize, upto: usize, skip: Option<u8>) -> bool {
    may_act_walk(gx, q, upto, skip, false, &[])
}

/// D4106: `may_act_but` of a RIGHT merge's outer (pipeline `q` before its merge op `upto`, the merge's own inner slot `skip`
/// left out) with the pipelines `ran` left out, which PG ran to their end at the merge's first outer fetch (lazy_outer.rs
/// `first_whole` of the outer's source): those act before PG's first NULLS LAST NULL key as v4's do, so the outer acts past
/// that key only through what runs per outer row after them (D1289).
pub(crate) fn outer_may_act_after(gx: &Gx<'_, '_>, q: usize, upto: usize, skip: Option<u8>, ran: &[usize]) -> bool {
    may_act_walk(gx, q, upto, skip, false, ran)
}

/// `may_act_but` for pipelines that fill a merge's inner run, a `user_sort` among them acting too: PG fetches the merge's
/// outer first, so an outer Sort runs whenever the merge does, while the inner's runs only once an outer row needs it.
pub(crate) fn inner_may_act(gx: &Gx<'_, '_>, q: usize, upto: usize, skip: Option<u8>) -> bool {
    may_act_walk(gx, q, upto, skip, true, &[])
}

/// D3799: `inner_may_act` of a whole inner fill `q` with the pipelines `ran` left out, which PG ran to their end before
/// the merge's first inner fetch (lazy_outer.rs `first_whole`: what the outer's first row ran).
pub(crate) fn inner_may_act_after(gx: &Gx<'_, '_>, q: usize, ran: &[usize]) -> bool {
    may_act_walk(gx, q, usize::MAX, None, true, ran)
}

/// USEROP: pipeline `p` sorts by a user btree family's comparator (a CompareFn key whose support function 1 is not built
/// in: effects' `builtin_fn`), user code at each comparison, which may raise.
pub(crate) fn user_sort(gx: &Gx<'_, '_>, p: &PipelineNode<'_>) -> bool {
    p.ops.iter().any(|op| matches!(op, Op::Sort(sd) if sd.keys.iter().any(|key| key.compare_fn() && gx.facts.key(gravity_decompose::catalog::KeyReq::sort(key.opno)).is_some_and(|o| !crate::effects::builtin_fn(o.cmp)))))
}

/// W8-EQUIVCLASS-SORTOP (equivclass#53): pipeline `k`'s ops before `upto` hold a merge join whose inner run is a Sort PG
/// cannot begin (a key with no order proc, decompose `SortKey::no_proc`): that merge raises at its first inner fetch
/// (ops/sort/sort_noproc.rs), so a merge reading `k`'s product as its inner waits for it as for any inner that may raise.
fn no_proc_reader(gx: &Gx<'_, '_>, k: usize, upto: usize) -> bool {
    let no_proc = |l: usize| gx.pipelines()[l].ops.iter().any(|o| matches!(o, Op::Sort(d) if d.keys.iter().any(|key| key.no_proc.is_some())));
    gx.pipelines()[k].ops.iter().take(upto).any(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, .. }) if gx.life_of(k, *slot).is_some_and(|l| no_proc(l.0 as usize))))
}

/// D3499: the pipelines of each data-modifying CTE's plan (its producer, `Gx::cte_write`, that holds a write, and all
/// that feeds it) that does not hold pipeline `q`. PG runs that ModifyTable to its end at the statement's end whatever
/// reads it (ExecPostprocessPlan), so a merge that reads it late or never changes only when it runs, an error's place.
fn drained_ctes<'g>(gx: &Gx<'g, '_>, q: usize) -> PgVec<'g, usize> {
    use crate::ops::mergejoin::lazy_outer::{lives, settled};
    let ps = gx.pipelines();
    let slots = |x: usize| ps[x].desc.consumes.iter().copied().chain(if let gravity_decompose::Source::Breaker { slot } = ps[x].source { Some(slot) } else { None });
    let mut out = PgVec::new_in(gx.mcx);
    let writes = |w: usize| ps[w].ops.iter().any(|op| matches!(op, Op::Write(x) if x.cmd != gravity_program::Cmd::Select));
    for w in gx.graph.init_plans.iter().map(|i| i.pipeline as usize).filter(|&w| w < ps.len() && w != q && gx.cte_write(w) && writes(w)) {
        // the write, each union's feeds and what each reads, transitively
        let mut plan = mcx::pg_vec![in gx.mcx; w];
        let mut i = 0;
        while let Some(&x) = plan.get(i) {
            i += 1;
            for y in settled(gx, x).into_iter().chain(lives(gx, x, slots(x))) {
                if !plan.contains(&y) {
                    plan.push_in(y);
                }
            }
        }
        if !plan.contains(&q) {
            out.extend_in(plan);
        }
    }
    out
}

fn may_act_walk(gx: &Gx<'_, '_>, q: usize, upto: usize, skip: Option<u8>, sorts: bool, ran: &[usize]) -> bool {
    let (mut todo, mut seen) = (mcx::pg_vec![in gx.mcx; q], drained_ctes(gx, q));
    seen.extend_in(ran.iter().copied().filter(|&x| x != q));
    while let Some(k) = todo.pop() {
        if seen.contains(&k) {
            continue;
        }
        seen.push_in(k);
        let mut feeds = PgVec::new_in(gx.mcx);
        if !pipe_quiet(gx, &gx.pipelines()[k], if k == q { upto } else { usize::MAX }, &mut feeds) || sorts && user_sort(gx, &gx.pipelines()[k]) || no_proc_reader(gx, k, if k == q { upto } else { usize::MAX }) {
            return true;
        }
        // D2026: every arm that fills a slot (lazy_outer.rs `fillers`) and a settled union's feeds, not the last arm alone
        todo.extend_in(feeds.into_iter().filter(|&s| k != q || Some(s) != skip).flat_map(|s| crate::ops::mergejoin::lazy_outer::fillers(gx, k, s)).chain(crate::ops::mergejoin::lazy_outer::settled(gx, k)));
    }
    false
}

/// mergejoin nest_inner.rs (D3634): filler `x` and all that feeds it call no user or volatile function, take no lock and
/// sort by no user comparator: its effect is at most a raise.
pub(crate) fn raise_only(gx: &Gx<'_, '_>, x: usize) -> bool {
    let (fed, user) = crate::frame::iface::fed_user_code(gx, x);
    !user && fed.into_iter().all(|q| !user_sort(gx, &gx.pipelines()[q]))
}
