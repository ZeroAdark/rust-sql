//! Reads of a settled union's admit (rerun/union_arms.rs `admit`, which calls them; the HOTSPOT rule keeps new
//! helpers of that file here): `full_read`, D1486's rule W (a union whose readers up to the statement's sink take
//! every row through quiet windows, filters and projections), `sink_read`, D1486's sink rule (a union the
//! statement's sink reads straight), `raise_only`, the window rule's arguments that may raise (D1486 b07),
//! `init_pruned`, a run-time pruning over InitPlans' outputs only (D1486 rnd-0098), `spool_read`, D1486's spool rule
//! (a CTE's union its one reader, or the reader PostgreSQL reads first (D3216), reads whole through nest joins,
//! keyed ones too (D3347, `spool_build`), into a sort), `every_read`, D1486's every-reader rule (each of a CTE's
//! readers reads its union whole, or as a hash join's outer over a whole build of it: D3261), `drained_write`, D1486's
//! drained-write rule (a data-modifying CTE's write over the union, which PostgreSQL runs to its end: D3499),
//! `written_arms`, its mirror (every arm reads such a write's store: D4202), and
//! `calm_nest`, rule A's nest whose per-row work may only raise (D1486 domain#507). A feed's index key computed from an expression
//! (D899's late word) needs no wall here since D2736's fix (gravity_parallel `with_late`, `pull_late`). Registers
//! nothing. Owner: append (partition_prune#794); `sink_read`, `raise_only`, `init_pruned`, `spool_read`,
//! `every_read`, `drained_write`, `written_arms` and `calm_nest`: window.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::graph::ParamOwner;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_program::{Cmd, LifeId};
use mcx::{CollectIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::graph::{EndKind, Gx};
use crate::effects::visible::material::quiet_nest;
use crate::rerun::union_arms::{quiet_in, reader, sort_reads};
use crate::effects::visible::union::{calm_emit, calm_in, calm_write, chain_calm, raise_only_in, steps_calm, written};
pub(crate) use crate::effects::visible::union::{calm_nest, raise_only};

/// Rule W (partition_prune#794): PostgreSQL reads every arm to its end when every reader up to the statement's sink takes
/// every row, and what it computes between two arms' rows shows nothing. The emit (quiet filters and projections over the
/// Union) fills a WindowAgg's partition buffer; that window has no run condition (which may end PostgreSQL's read early),
/// and its qual, arguments, FILTERs and functions are quiet (ranking functions, builtin aggregates that raise on no
/// input); each pipeline from the window's up to the sink holds only such windows (a stacked window reads its
/// buffer), quiet filters and projections, and an outer union's arm marker, and ends in the statement's sink (no LIMIT,
/// sort, join or other breaker on the way). PostgreSQL's WindowAgg hands a partition's rows on before it reads the next
/// arm, but that work raises nothing and calls no user or VOLATILE code, so the fill's arms make PostgreSQL's reads,
/// calls and first error, in Append order, before any row leaves (an error discards the rows PostgreSQL sent before it).
/// That holds only when the statement runs to its end: a cursor's FETCH, a protocol portal's row limit or an SPI caller
/// may stop it sooner, where PostgreSQL reads fewer arms; so the program is served only to a reader that takes every row
/// (the emit's `source`: `Px::whole_read`, D1922) and the entry delegates it to the row executor elsewhere.
pub(crate) fn full_read(gx: &Gx<'_, '_>, emit: usize) -> bool {
    let ps = gx.pipelines();
    let q = |x: Node<'_>| quiet_in(gx, x, false);
    let emit_ops = ps[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Filter(x) => q(*x),
        Op::Project(l) => l.iter().all(q),
        _ => false,
    });
    let Some(mut c) = reader(gx, emit).filter(|&c| matches!(ps[c].ops.first(), Some(Op::Window(_)))) else { return false };
    if !emit_ops {
        return false;
    }
    // a window reads its whole input when its reader takes every row (no run condition), and its per-row work shows
    // nothing: a quiet qual, arguments and FILTERs, and functions that raise on no input (a ranking function or a builtin
    // aggregate: decompose facts/window.rs `reads_whole_in`'s own test, effects `agg_oid_quiet`)
    let window = |d: &gravity_decompose::ops::window::WindowDesc<'_>| {
        use gravity_decompose::ops::window::WinFnKind;
        d.plan.runCondition.is_nil() && d.plan.plan.qual.iter().all(q) && d.functions.iter().all(|f| {
            let fnq = match f.kind {
                WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank | WinFnKind::PercentRank | WinFnKind::CumeDist => true,
                WinFnKind::Aggregate => gravity_decompose::effects::agg_oid_quiet(f.func.winfnoid, || f.func.args.iter().next().is_some_and(gravity_decompose::effects::num_bounded)),
                _ => false,
            };
            fnq && f.func.args.iter().all(q) && f.func.aggfilter.is_none_or(q)
        })
    };
    let init = |x: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == x);
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == x));
    loop {
        let ops_ok = ps[c].ops.iter().all(|op| match op {
            Op::Window(d) => window(d),
            Op::Filter(x) => q(*x),
            Op::Project(l) => l.iter().all(q),
            Op::Append(AppendDesc::Arm { prune: None, .. }) => true,
            _ => false,
        });
        if !ops_ok || init(c) || rerun(c) {
            return false;
        }
        match gx.end_kind(c) {
            crate::graph::EndKind::Sink => return true,
            // a stacked WindowAgg's partition buffer, its one reader opening with the window
            crate::graph::EndKind::Breaker(BreakerKind::Materialize) => match reader(gx, c).filter(|&r| r > c && matches!(ps[r].ops.first(), Some(Op::Window(_)))) {
                Some(r) => c = r,
                None => return false,
            },
            _ => return false,
        }
    }
}

/// D1486: the sink rule. The emit's ops are quiet filters,
/// projections and quiet nests (`quiet_nest`), and its rows go to the statement's sink through quiet filters and
/// projections alone: the emit is the root (a SELECT's), or it ends in a Materialize (a CTE's spool) whose one reader is
/// the root, reading it as its source. PostgreSQL's executor then reads the union row after row as the client takes them,
/// and when the client takes every row it reads every arm to its end, running only those quiet ops between two arms'
/// rows: the eager fill shows nothing it does not. Like rule C, that holds only when the statement runs to its end, so the
/// program is served only to a reader that takes every row (`Px::whole_read`, the third value of `admit`).
pub(crate) fn sink_read(gx: &Gx<'_, '_>, emit: usize) -> bool {
    let ps = gx.pipelines();
    let q = |x: Node<'_>| quiet_in(gx, x, false);
    let quiet = |op: &Op<'_>| match op {
        Op::Filter(x) => q(*x),
        Op::Project(l) => l.iter().all(q),
        _ => false,
    };
    let emit_ops = ps[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Join(JoinDesc::Nest { params, qual, inner, .. }) => quiet_nest(gx, Some(emit), params, qual, inner),
        _ => quiet(op),
    });
    let root = gx.graph.root as usize;
    let life = Some(LifeId(emit as u16));
    let to_sink = match gx.end_kind(emit) {
        EndKind::Sink => emit == root,
        EndKind::Breaker(BreakerKind::Materialize) => reader(gx, emit).is_some_and(|c| {
            c == root && gx.end_kind(c) == EndKind::Sink && matches!(ps[c].source, Source::Breaker { slot } if gx.life_of(c, slot) == life) && ps[c].ops.iter().all(quiet)
        }),
        _ => false,
    };
    emit_ops && to_sink
}

/// D1486: a settled union's run-time (exec) pruning whose every parameter is an InitPlan's
/// output (append/prune.rs `exec_over_init_plans`' test) is decided once per execution, as an initial one is: PostgreSQL's
/// first ExecAppend runs ExecFindMatchingSubPlans over the InitPlans' values and never re-decides it unless a rescan
/// changes a parameter, which an InitPlan's output does not. So each feed takes its child's skip bit as for an initial
/// pruning (`step`'s Skip; the src.prune protocol decides exec steps after the InitPlans, or at the first arm's start for
/// an in-program one: exec ops/append/prune_exec.rs), and a pruned arm runs empty where PostgreSQL never opens it. A
/// feed a rerun set re-runs keeps refusing (its pruning would be the set's, per re-run).
pub(crate) fn init_pruned(gx: &Gx<'_, '_>, d: &PruneDesc<'_>, feeds: &[usize]) -> bool {
    let inits = &gx.graph.init_plans;
    let over_inits = d.info.prune_infos.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).all(|r| {
        let Some(r) = r.as_partitioned_rel_prune_info() else { return false };
        let mut x = r.execparamids.next_member(-1);
        while x >= 0 {
            if !inits.iter().any(|i| i.param == x) {
                return false;
            }
            x = r.execparamids.next_member(x);
        }
        true
    });
    let rerun = |f: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == f));
    over_inits && !feeds.iter().any(|&f| rerun(f))
}

/// D1486: the spool rule. The emit (filters and projections over the Union that call nothing and may only raise:
/// `calm`) fills a Materialize spool, a CTE's, whose one reader pipeline reads it from its start as its source.
/// PostgreSQL's CteScan there pulls the CTE row after row, and that pipeline reads it to its end before its first
/// row leaves when it holds only filters, projections and nest joins (a NestLoop reads its outer to its end whatever
/// its join type; its inner, the CTE's other scan or one heap / index scan, re-reads at every outer row; a keyed
/// one, D3347, probes a table its build reads from the spool once: `spool_build`) and ends in a full sort or a top-k
/// that reads (`sort_reads`; no presorted prefix) over builtin comparators, and each pipeline from there to the
/// statement's root pulls at least one row (filters, projections, sorts that read, constant LIMITs of a row or more,
/// and joins over that run as their outer: D3216 `runs_first`) and ends in a breaker that reads its whole input, or
/// is the root. D3216: a spool several pipelines read takes the one PostgreSQL reads whole first (`first_read`). So
/// PostgreSQL reads every arm to its end, in Append order, whenever the statement returns rows; the eager fill reads
/// the same arms. Between two arms' rows PostgreSQL runs only that reader's work, which calls no user or VOLATILE
/// code and may only raise (`raise_only`): run after the arms instead, over the same rows, it raises where
/// PostgreSQL does and changes only which of two errors comes first (`raise_only`: which error is not part of the
/// answer). A hash join (an empty build ends PostgreSQL's outer read), a gate, a LIMIT before the breaker, a SubPlan,
/// an aggregate end other than `folds`', or a pipeline a rerun set re-runs keeps refusing.
pub(crate) fn spool_read(gx: &Gx<'_, '_>, emit: usize) -> bool {
    let ps = gx.pipelines();
    let calm = |x: Node<'_>| quiet_in(gx, x, false) || raise_only(gx, x);
    // D3373: a nest inner's work, the nest's own parameters bound (`raise_only_in`); its LIMIT (a LATERAL subquery's)
    // stops only that inner's read
    let calm_ops = |ops: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool| {
        let bound = |b: &gravity_decompose::Bound<'_>| match b {
            gravity_decompose::Bound::Expr(x) => calm(*x),
            _ => true,
        };
        ops.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            op => crate::frame::iface::limit_bounds(op).is_some_and(|(count, offset)| bound(count) && bound(offset)),
        })
    };
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == x));
    let life = Some(LifeId(emit as u16));
    if gx.end_kind(emit) != EndKind::Breaker(BreakerKind::Materialize) || rerun(emit) {
        return false;
    }
    let emit_ops = calm_emit(gx, emit, &calm);
    let Some(c) = reader(gx, emit).or_else(|| first_read(gx, emit)).filter(|&c| c > emit && !rerun(c)) else { return false };
    let reads_spool = |p: &PipelineNode<'_>| matches!(p.source, Source::Breaker { slot } if gx.life_of(c, slot) == life);
    if !emit_ops || !reads_spool(&ps[c]) {
        return false;
    }
    let builtin = |opno: u32| opno == 0 || gravity_decompose::effects::builtin_oper(opno).is_some();
    let inner_ok = |p: &PipelineNode<'_>, calm: &dyn Fn(Node<'_>) -> bool| {
        let src = match &p.source {
            Source::Breaker { .. } => reads_spool(p),
            Source::Range { am: gravity_decompose::Am::Heap, .. } => true,
            Source::Index { am: gravity_decompose::Am::Heap, keys, recheck, .. } => keys.iter().all(|k| calm(k.value) && builtin(k.opno)) && recheck.iter().all(calm),
            _ => false,
        };
        let own = |s: &u8| matches!(p.source, Source::Breaker { slot } if slot == *s);
        src && p.desc.consumes.iter().all(own) && calm_ops(&p.ops, calm)
    };
    let last = ps[c].ops.len().saturating_sub(1);
    let mut owed = false;
    let reads = ps[c].ops.iter().enumerate().all(|(j, op)| match op {
        Op::Filter(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        Op::Join(JoinDesc::Nest { params, qual, inner, .. }) => {
            let own = |id: i32| params.iter().any(|p| p.as_nest_loop_param().is_some_and(|p| p.paramno == id));
            let calm_in = |x: Node<'_>| {
                gx.fx(x).quiet_in(&crate::effects::raise_free::Reading::union_arms(&own)) || raise_only_in(gx, x, &own)
            };
            params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| calm(p.paramval))) && qual.iter().all(|x| calm_in(x)) && inner_ok(inner, &calm_in)
        }
        // D3347: a keyed nest over the CTE's other scan (decompose self_spool.rs `keyed`, B6) probes the table its own build
        // pipeline fills from the spool once (`spool_build`), at the first outer row or before it, where PostgreSQL's
        // first inner scan reads the CTE to its end; its keys and qual run per outer row as the Nest's join filter does
        Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, clauses, keys, qual, outer_first: true, catalog: true, .. }) => {
            clauses.iter().chain(keys.iter()).chain(qual.iter()).all(calm) && gx.life_of(c, *slot).is_some_and(|l| spool_build(gx, emit, l.0 as usize, c))
        }
        // the breaker's own sort, its last op: a full sort or a top-k over no presorted prefix (an incremental sort or a
        // walk hands rows on before its input's end)
        Op::Sort(d) => {
            j == last && d.presorted == 0 && matches!(d.form, SortForm::Runs | SortForm::TopK { .. }) && d.keys.iter().all(|k| builtin(k.opno)) && sort_reads(d, &mut owed)
        }
        _ => false,
    });
    let sorted = |q: usize| matches!(gx.end_kind(q), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK));
    // from the sort up to the root: each reader pulls a row and reads its input whole (`pulls_root`)
    reads && !owed && (sorted(c) || folds(gx, c, &calm, false)) && pulls_root(gx, c)
}

/// Pipeline `x` is a member of a rerun set (a nest's or a SubPlan's re-open re-runs it).
fn rerun_member(gx: &Gx<'_, '_>, x: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == x))
}

/// The spool rules' chain from reader `c` (which reads its input whole, or is the root) up to the statement's root:
/// each reader pulls at least one row (`unconditional`'s chain: filters, projections, sorts that read, constant LIMITs
/// of a row or more, and joins over that run as their outer: D3216 `runs_first`) and ends in a breaker that reads its
/// whole input, or is the root. D3261: an arm of a streamed union (`arms_end`) continues at the last arm, whose life
/// the arms' shared breaker hands its reader: PostgreSQL's Append under that breaker reads every arm, one after
/// another, to its end.
fn pulls_root(gx: &Gx<'_, '_>, c: usize) -> bool {
    let ps = gx.pipelines();
    let root = gx.graph.root as usize;
    let mut q = c;
    while q != root {
        let q_end = arms_end(gx, q).unwrap_or(q);
        let Some(r) = reader(gx, q_end).filter(|&r| r > q_end && !rerun_member(gx, r)) else { return false };
        let mut next = false;
        let outer = runs_first(gx, r, q_end);
        let fetches = ps[r].ops.iter().all(|op| match op {
            Op::Filter(_) | Op::Project(_) => true,
            Op::Sort(d) => sort_reads(d, &mut next),
            // D3216: a join whose outer `q`'s run is (`runs_first`): its node fetches that outer before anything else
            Op::Join(JoinDesc::Nest { .. } | JoinDesc::Merge { .. } | JoinDesc::Probe { outer_first: true, .. }) => outer,
            // D3261: a streamed union's arm marker (no work of its own; `arms_end` reads the arms)
            Op::Append(AppendDesc::Arm { prune: None, .. }) => true,
            // a constant LIMIT of one row or more (ops/limit `const_bound`)
            op => crate::ops::limit::lower::const_bound(op),
        });
        if !fetches || next {
            return false;
        }
        if r != root && !matches!(gx.end_kind(r), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct)) {
            return false;
        }
        q = r;
    }
    true
}

/// D3261: pipeline `q` is an arm of a streamed union (decompose append.rs `UnionRec`: arm `i` the `i`-th pipeline of
/// `pushed`, each holding one unpruned arm marker) whose arms all end in `q`'s breaker: the last arm, whose life that
/// breaker's reader reads. PostgreSQL's Append there runs arm after arm, each to its end, as the breaker reads.
fn arms_end(gx: &Gx<'_, '_>, q: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let u = gx.graph.unions.iter().find(|u| (u.pushed.start as usize..u.pushed.end as usize).contains(&q))?;
    let arms = u.pushed.start as usize..u.pushed.end as usize;
    let end = |x: usize| crate::frame::iface::breaker_of(&ps[x].desc).map(|b| (b.kind, b.slot));
    let marker = |x: usize| {
        let mut ms = ps[x].ops.iter().filter_map(|o| if let Op::Append(AppendDesc::Arm { i, n, prune, .. }) = o { Some((*i as usize, *n as usize, prune.is_none())) } else { None });
        matches!((ms.next(), ms.next()), (Some((i, n, true)), None) if i == x - arms.start && n == arms.len())
    };
    (arms.end <= ps.len() && end(q).is_some() && arms.clone().all(|x| end(x) == end(q) && marker(x))).then(|| arms.end - 1)
}

/// D3261: the every-reader rule. The emit (`calm_emit`) fills a CTE's spool that no nest inner reads, and every
/// pipeline reading it reads it from its start as its source, outside any rerun set, either whole (`whole_reader`: a
/// full sort or top-k that reads, a `folds` aggregate, or a hash build, over filters and projections that call nothing
/// and may only raise) or as a hash join's outer (`probe_reader`) whose first join probes a whole reader's build. So
/// PostgreSQL's first pull of the CTE, whichever reader makes it, goes on to its end: a whole reader's read, or a probe
/// reader's outer fetch (nodeHashjoin.c's outer-first path; finding no row reads every arm) and then that join's build.
/// Between two arms' rows PostgreSQL runs only that reader's work, which calls nothing and may only raise: run after
/// the arms instead, it raises where PostgreSQL does and changes only which of two errors comes first (`raise_only`).
/// And PostgreSQL pulls the CTE whenever the statement returns rows: a whole reader ending in a sort or a fold, or a
/// probe reader each of whose build-first joins builds from the spool (an empty build of another input ends the read
/// before any pull), reaches the root (`pulls_root`). So PostgreSQL reads every arm to its end, in Append order,
/// whenever the statement returns rows, as the eager fill does (D3261: a CTE of grouped arms read by two aggregates, or
/// by hash joins of the CTE with itself under enable_nestloop off).
pub(crate) fn every_read(gx: &Gx<'_, '_>, emit: usize) -> bool {
    let ps = gx.pipelines();
    let calm = |x: Node<'_>| quiet_in(gx, x, false) || raise_only(gx, x);
    let life = LifeId(emit as u16);
    if gx.end_kind(emit) != EndKind::Breaker(BreakerKind::Materialize) || rerun_member(gx, emit) || !calm_emit(gx, emit, &calm) {
        return false;
    }
    if ps.inners().iter().any(|(_, p)| p.desc.consumes_from.contains(&life)) {
        return false;
    }
    let reads = |x: usize| ps[x].desc.consumes.iter().any(|&s| gx.life_of(x, s) == Some(life));
    let readers: PgVec<'_, usize> = (0..gx.graph.pipelines.len()).filter(|&x| x != emit && reads(x)).collect_in(gx.mcx);
    let src = |x: usize| x > emit && !rerun_member(gx, x) && matches!(ps[x].source, Source::Breaker { slot } if gx.life_of(x, slot) == Some(life));
    let whole = |x: usize| src(x) && whole_reader(gx, x, &calm);
    let build = |x: usize, s: u8| gx.life_of(x, s).map(|l| l.0 as usize).filter(|&b| b < ps.len() && whole(b) && gx.end_kind(b) == EndKind::Breaker(BreakerKind::HashBuild));
    let probe = |x: usize| src(x).then(|| probe_reader(gx, x, &calm, &|s| build(x, s).is_some())).flatten();
    // a whole reader's end other than a build (which runs only when its join pulls it)
    let reads_first = |x: usize| matches!(gx.end_kind(x), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg));
    !readers.is_empty()
        && readers.iter().all(|&x| whole(x) || probe(x).is_some())
        && readers.iter().any(|&x| (whole(x) && reads_first(x) || probe(x) == Some(true)) && pulls_root(gx, x))
}

/// D3499: the drained-write rule. The emit is a data-modifying CTE's write over the union (a CTE's producer,
/// `Gx::cte_write`, that holds a write), which PostgreSQL runs to its end at the statement's end whatever its readers
/// read (ExecPostprocessPlan), so it reads every arm to its end, in Append order, whenever the statement completes. Each arm reads a heap through steps that call
/// nothing and may only raise (`steps_calm`), under the statement's snapshot, so no arm sees the write's rows or depends
/// on when it runs. Between two arms' rows PostgreSQL runs the emit's calm filters and projections and that row's write,
/// which runs no user code before the next row: no BEFORE ROW, INSTEAD OF or statement trigger of the command on any
/// written relation (an AFTER ROW one waits for the query's end, after every arm), no WITH CHECK OPTION or policy
/// check, no ON CONFLICT, no MERGE, no row moved to another partition; the generation expressions and CHECK constraints
/// it computes PostgreSQL takes as immutable. Run before the writes instead, an arm raises where PostgreSQL does and
/// changes only which of two errors comes first (D3261's reading; isolation eval-plan-qual's complexpartupdate).
pub(crate) fn drained_write(gx: &Gx<'_, '_>, emit: usize, feeds: &[usize]) -> bool {
    let ps = gx.pipelines();
    let calm = |x: Node<'_>| calm_in(gx, x);
    let writes = ps[emit].ops.iter().any(|op| matches!(op, Op::Write(w) if w.cmd != Cmd::Select));
    gx.cte_write(emit) && writes && !rerun_member(gx, emit) && !feeds.is_empty()
        && feeds.iter().all(|&f| f < ps.len() && !rerun_member(gx, f) && steps_calm(gx, &ps[f]))
        && ps[emit].ops.iter().all(|op| match op {
            Op::Append(AppendDesc::Union { prune: None, .. }) => true,
            Op::Filter(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            Op::Write(w) => calm_write(gx, w),
            _ => false,
        })
}

/// D4202, the written-arms rule (D3499's mirror: the writes under the union, not over it). Every feed reads a
/// data-modifying CTE's store (`inner_arms::cte_arm`: filled once) through calm steps, and that CTE's producer
/// writes (`Gx::cte_write`): PostgreSQL runs such a write to its end at the statement's end whatever its readers read
/// (ExecPostprocessPlan), so the store holds every RETURNING row whenever the statement completes, and a fill ahead of
/// PostgreSQL's pulls moves only when the writes make their rows. Each write is `calm_write`; its producer's source,
/// filters, projections and set functions, and what feeds it (`chain_calm`), call nothing that acts, so no call of it
/// changes place among the statement's (a nextval in its input or a serial column's default refuses); and no other
/// write of the statement writes a relation it writes (two writes of one row: PostgreSQL keeps the first, which the
/// order decides). Every read is under the statement's snapshot, so no arm or reader sees a write's rows. A raise in
/// a write changes only which of two errors comes first.
pub(crate) fn written_arms(gx: &Gx<'_, '_>, feeds: &[usize]) -> bool {
    let ps = gx.pipelines();
    let calm = |x: Node<'_>| calm_in(gx, x);
    let alone = |w: &WriteDesc<'_>| written(w, &mut |r| ps.iter().all(|p| p.ops.iter().all(|op| match op {
        Op::Write(x) if x.cmd != Cmd::Select && !core::ptr::addr_eq(x, w) => written(x, &mut |y| y != r),
        _ => true,
    })));
    let producer = |l: usize| {
        let p = &ps[l];
        let src = matches!(p.source, Source::Range { am: gravity_decompose::Am::Heap, .. } | Source::Breaker { .. } | Source::Const);
        let ops = p.ops.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            Op::Srf(d) => d.functions.iter().all(|f| calm(f.node)) && d.tlist.iter().all(calm),
            Op::Write(w) => w.cmd != Cmd::Select && calm_write(gx, w) && alone(w),
            _ => false,
        });
        let slot = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
        let fed = crate::ops::hashjoin::build_class::feed(gx, l, p.desc.consumes.iter().copied().chain(slot));
        gx.cte_write(l) && !rerun_member(gx, l) && src && ops && gx.group_at(l).is_none()
            && p.ops.iter().any(|op| matches!(op, Op::Write(w) if w.cmd != Cmd::Select))
            && fed.into_iter().all(|x| chain_calm(gx, x))
    };
    let arm = |f: usize| {
        let Some(Source::Breaker { slot }) = (f < ps.len()).then(|| &ps[f].source) else { return false };
        !rerun_member(gx, f) && steps_calm(gx, &ps[f]) && crate::rerun::inner_arms::cte_arm(gx, f)
            && gx.life_of(f, *slot).is_some_and(|l| (l.0 as usize) < ps.len() && producer(l.0 as usize))
    };
    !feeds.is_empty() && feeds.iter().all(|&f| arm(f))
}

/// D3261: reader `x` reads its source whole before its first row leaves: filters and projections `calm` takes, ending
/// in a full sort or a top-k that reads (`sort_reads`; no presorted prefix, builtin comparators), a `folds` aggregate
/// (its builtin transitions may raise too), or a hash build of calm keys (MultiExecHash reads its whole input).
fn whole_reader(gx: &Gx<'_, '_>, x: usize, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let builtin = |opno: u32| opno == 0 || gravity_decompose::effects::builtin_oper(opno).is_some();
    let last = ps[x].ops.len().saturating_sub(1);
    let mut owed = false;
    let ops = ps[x].ops.iter().enumerate().all(|(j, op)| match op {
        Op::Filter(n) => calm(*n),
        Op::Project(l) => l.iter().all(|n| calm(n)),
        Op::Join(JoinDesc::Build { keys, .. }) => j == last && keys.iter().all(|n| calm(n)),
        Op::Sort(d) => j == last && d.presorted == 0 && matches!(d.form, SortForm::Runs | SortForm::TopK { .. }) && d.keys.iter().all(|k| builtin(k.opno)) && sort_reads(d, &mut owed),
        _ => false,
    });
    let ends = match gx.end_kind(x) {
        EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK) => matches!(ps[x].ops.last(), Some(Op::Sort(_))),
        EndKind::Breaker(BreakerKind::HashBuild) => matches!(ps[x].ops.last(), Some(Op::Join(JoinDesc::Build { .. }))),
        _ => folds(gx, x, calm, true),
    };
    ops && !owed && ends
}

/// D3261: reader `x` reads its source as the outer of its first join, a hash join whose build `built` takes (a whole
/// reader of the same spool), past filters and projections `calm` takes: PostgreSQL fetches that outer only from that
/// join, at most one row before the build reads the spool whole (the outer-first path), and the rest after. Then hash
/// joins, filters and projections, ending in a full sort or a top-k that reads, a fold, or the root's sink.
/// Some(whether a pull of `x` pulls the spool: every join that builds first builds from it, `built`), else None.
fn probe_reader(gx: &Gx<'_, '_>, x: usize, calm: &dyn Fn(Node<'_>) -> bool, built: &dyn Fn(u8) -> bool) -> Option<bool> {
    let ops = &gx.pipelines()[x].ops;
    let j1 = ops.iter().position(|op| matches!(op, Op::Join(_)))?;
    let Op::Join(JoinDesc::Probe { slot, .. }) = &ops[j1] else { return None };
    let before = ops[..j1].iter().all(|op| match op {
        Op::Filter(n) => calm(*n),
        Op::Project(l) => l.iter().all(|n| calm(n)),
        _ => false,
    });
    if !before || !built(*slot) {
        return None;
    }
    let last = ops.len() - 1;
    let mut owed = false;
    let rest = ops[j1..].iter().enumerate().all(|(j, op)| match op {
        Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Probe { .. }) => true,
        Op::Sort(d) => j1 + j == last && d.presorted == 0 && sort_reads(d, &mut owed),
        _ => false,
    });
    let pulls = ops.iter().all(|op| !matches!(op, Op::Join(JoinDesc::Probe { slot, outer_first: false, .. }) if !built(*slot)));
    let ends = x == gx.graph.root as usize || matches!(gx.end_kind(x), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg));
    Some(rest && !owed && ends && pulls)
}

/// D3373: `spool_read`'s reader `c` may end in a plain or hashed aggregate instead of a sort: it reads its whole
/// input before its first row too, and between two input rows it runs its keys' images and its transitions, builtin
/// aggregates that raise on no input (`effects::agg_quiet`; no FILTER, DISTINCT, ORDER BY or direct argument) over keys
/// and arguments that call nothing and may only raise (`calm`). D3261 (`raising`, `every_read`'s whole readers): or a
/// builtin plain aggregate (`effects::builtin_plain_agg`) whose transitions may only raise as well (`raise_only` over
/// the Aggref: a numeric sum's overflow), the same reading as its arguments'.
fn folds(gx: &Gx<'_, '_>, c: usize, calm: &dyn Fn(Node<'_>) -> bool, raising: bool) -> bool {
    let mut gs = gx.graph.groups.iter().filter(|g| g.producer == LifeId(c as u16));
    let (Some(g), None) = (gs.next(), gs.next()) else { return false };
    let agg = |n: Node<'_>| {
        n.as_aggref().is_some_and(|a| {
            let raises = raising && crate::effects::builtin_plain_agg(n) && raise_only(gx, n);
            (gravity_decompose::effects::agg_quiet(a) || raises) && a.aggfilter.is_none() && a.aggdistinct.is_nil() && a.aggorder.is_nil() && a.aggdirectargs.is_nil() && a.args.iter().all(calm)
        })
    };
    matches!(gx.end_kind(c), EndKind::Breaker(BreakerKind::PlainAgg | BreakerKind::HashAgg))
        && g.set.is_none()
        && g.two_pass.is_none()
        && g.keys.iter().all(|key| calm(key.expr.node()))
        && g.aggs.iter().all(|a| agg(a.node))
}

/// D3216: pipeline `r` reads `q`'s run as its source, the outer of each join `r` holds: a NestLoop and a Merge Join fetch
/// their outer before their inner (nodeNestloop.c, nodeMergejoin.c EXEC_MJ_INITIALIZE_OUTER), and so does a Hash Join on
/// PostgreSQL's outer-first path (`outer_first`), so `r`'s first row asks `q` for its first row before any of its inners.
fn runs_first(gx: &Gx<'_, '_>, r: usize, q: usize) -> bool {
    matches!(gx.pipelines()[r].source, Source::Breaker { slot } if gx.life_of(r, slot) == Some(LifeId(q as u16)))
}

/// D3216: the spool rule's reader when several graph pipelines read the spool: the one, `c`, PostgreSQL reads whole before
/// any other. `c` reads the spool as its source and its one reader `r` reads `c`'s run as its source (`runs_first`); every
/// other reader reads the spool as its source too and is the inner of one of `r`'s Merge Joins or outer-first Hash Joins,
/// read by nothing else, or is `r` itself through its nests; every nest inner reading the spool is one of `c`'s or `r`'s. `r`'s first row asks `c` first, and
/// `c` reads the spool to its end before its first row leaves (`spool_read`'s own test of `c`), so the other readers then
/// read the CTE's tuplestore, whole: they compute no arm row, and PostgreSQL's work between two arms' rows is still `c`'s.
fn first_read(gx: &Gx<'_, '_>, emit: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let life = LifeId(emit as u16);
    let src = |x: usize| matches!(ps[x].source, Source::Breaker { slot } if gx.life_of(x, slot) == Some(life));
    let readers: PgVec<'_, usize> = (0..gx.graph.pipelines.len()).filter(|&x| x != emit && ps[x].desc.consumes.iter().any(|&s| gx.life_of(x, s) == Some(life))).collect_in(gx.mcx);
    let inner_of = |r: usize, x: usize| {
        ps[r].ops.iter().any(|op| match op {
            Op::Join(JoinDesc::Merge { slot, .. } | JoinDesc::Probe { slot, outer_first: true, .. }) => gx.life_of(r, *slot) == Some(LifeId(x as u16)),
            _ => false,
        })
    };
    let nests = ps.inners(); // alloc: Program
    readers.iter().copied().find(|&c| {
        reader(gx, c).is_some_and(|r| {
            src(c)
                && runs_first(gx, r, c)
                && readers.iter().all(|&x| x == c || x == r || src(x) && reader(gx, x) == Some(r) && inner_of(r, x) || spool_build(gx, emit, x, c))
                && nests.iter().filter(|(_, p)| p.desc.consumes_from.contains(&life)).all(|((k, _), _)| *k == c || *k == r)
        })
    })
}

/// D3347 (the spool rule's keyed nests): pipeline `x` builds the table a keyed nest of reader `c` probes (decompose
/// self_spool.rs `keyed`, B6: the Nest's inner a CTE Scan of the same CTE), straight from the spool `emit` fills: the
/// spool is its source, it reads no other slot, it ends in the build, its filters, projections and build keys may only
/// raise (`raise_only`) and call nothing that acts, and `c` alone reads its table, through a catalog probe on
/// PostgreSQL's outer-first path. It runs once, at `c`'s first row that reaches the probe (D824's lazy group) or before
/// `c` (a Safe side in program order), reading the spool to its end, where PostgreSQL's first inner scan of the Nest
/// reads the CTE to its end; and when no row reaches the probe, `c`'s own read of the spool has read it to its end, as
/// PostgreSQL's outer CTE Scan has. So every arm still runs whole, in Append order, whenever the statement returns
/// rows, and between two arms' rows only the build's or `c`'s calm work runs.
fn spool_build(gx: &Gx<'_, '_>, emit: usize, x: usize, c: usize) -> bool {
    let ps = gx.pipelines();
    if x >= ps.len() || x == c || x <= emit || reader(gx, x) != Some(c) || gx.end_kind(x) != EndKind::Breaker(BreakerKind::HashBuild) {
        return false;
    }
    let calm = |n: Node<'_>| quiet_in(gx, n, false) || raise_only(gx, n);
    let p = &ps[x];
    let src = matches!(p.source, Source::Breaker { slot } if gx.life_of(x, slot) == Some(LifeId(emit as u16)) && p.desc.consumes.iter().all(|&s| s == slot));
    let last = p.ops.len().saturating_sub(1);
    let ops = matches!(p.ops.last(), Some(Op::Join(JoinDesc::Build { .. })))
        && p.ops.iter().enumerate().all(|(i, op)| match op {
            Op::Filter(n) => calm(*n),
            Op::Project(l) => l.iter().all(calm),
            Op::Join(JoinDesc::Build { keys, .. }) => i == last && keys.iter().all(calm),
            _ => false,
        });
    let probed = ps[c].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, outer_first: true, catalog: true, .. }) if gx.life_of(c, *slot) == Some(LifeId(x as u16))));
    src && ops && probed
}

/// The pipeline that fills the plan Material spool `slot` nest inner `inner` of pipeline `k` reads (`quiet_nest`'s
/// `spool_fill` without its one-scan test): the life the inner reads, else the slot's nearest producer before `k`, ending
/// in a Materialize.
pub(crate) fn fill_of(gx: &Gx<'_, '_>, k: usize, inner: &PipelineNode<'_>, slot: u8) -> Option<usize> {
    let (d, ps) = (&inner.desc, gx.pipelines());
    let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
    let f = life.map(|l| l.0 as usize).or_else(|| (0..k).rev().find(|&q| ps[q].desc.produces() == Some(slot)))?;
    (f < k && ps[f].desc.produces() == Some(slot) && gx.end_kind(f) == EndKind::Breaker(BreakerKind::Materialize)).then_some(f)
}

