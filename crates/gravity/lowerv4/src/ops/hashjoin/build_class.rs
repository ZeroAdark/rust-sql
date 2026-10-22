//! The side classification of a join's build and outer side (D237, D276, D762, op 14, D824): what each pipeline's own ops
//! and feeders may raise or call, whether a raising build's error is held for the probe's first row, and which outer
//! producers the runner may skip after an empty build. Split from join/build.rs (D824). Owner M1-JOIN.
use gravity_decompose::catalog::IndexAm;
use gravity_decompose::graph::Source;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::graph::Gx;

pub(crate) use crate::effects::visible::hashjoin::{own_side, quiet_statement, shared_cte_raises, side, side_by, side_by_at};
use crate::effects::visible::hashjoin::{keyed_side, own_side_by, own_side_nests, side_calls};

/// What PG's lazy paths could skip (D237): `Safe` when nothing it runs can raise or have an effect, `UserFn` when
/// it calls a non-builtin function or a VOLATILE one (D1005: nextval, random; no deferral makes its effects exact:
/// expr.volatile, M2) or locks rows (V4R-512: a lock's effects outlive a held error), else `Raises`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Side {
    Safe,
    Raises,
    UserFn,
}

/// D3178: pipeline `x`, run in program order between a lazy group's first member and its reader outside both sides (D824's
/// `lazy_group_by`: a Material under a nest above a merge join's lazy inner), runs ahead of PG's order or where PG never
/// runs it, so it must raise and act nowhere: `own_side`'s Safe, with each nest step read by its F5 facts as D1108 reads
/// one (`effects::nest_step_quiet`: an inner that raises nothing and calls nothing VOLATILE, a raise-free qual and
/// parameters) where `own_side` reads every nest step as a raise (D2107). D3374: a Limit of settled bounds reads as
/// quiet too, where `own_side` reads every Limit as a raise.
fn between_safe(gx: &Gx<'_, '_>, x: usize) -> bool {
    let ops = &gx.pipelines()[x].ops;
    let upto = ops.len() - matches!(ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) as usize;
    own_side_nests(gx, x, &|n| side_calls(gx, n), None, upto, true) == Side::Safe
}

/// The pipelines that feed pipeline `k` through `slots`, transitively (each slot's producing life and all it consumes; a settled union's emit, `k` or one on the way: every arm; a streamed union's arm on the way: every arm of its run, `Gx::run_arms`, D2115).
pub(crate) fn feed<'g>(gx: &Gx<'g, '_>, k: usize, slots: impl Iterator<Item = u8>) -> PgVec<'g, usize> {
    let lives = |k: usize, s: u8| gx.life_of(k, s).map(|l| l.0 as usize).filter(|&l| l < gx.pipelines().len() && !gx.settled_work(k, s));
    // D3897 (D3893's `arms_at`): a settled union's record a nest or SubPlan inner embedded in `k` reads names its nearest
    // arm alone (`life_of`); every arm feeds it, as `union_feeds` reads a graph emit's
    let arms = |k: usize, s: u8| lives(k, s).filter(|&l| crate::rerun::union_arms::feed_of(&gx.pipelines()[l]) == Some(s)).map_or_else(|| PgVec::new_in(gx.mcx), |l| crate::rerun::union_join_arms::arms_upto(gx, l, s));
    let (mut out, mut todo): (PgVec<'g, usize>, PgVec<'_, usize>) = (PgVec::new_in(gx.mcx), slots.flat_map(|s| lives(k, s).into_iter().chain(arms(k, s))).chain(gx.union_feeds(k)).collect_in(gx.mcx));
    while let Some(p) = todo.pop() {
        if !out.contains(&p) {
            out.push_in(p);
            todo.extend_in(gx.pipelines()[p].desc.consumes.iter().flat_map(|&s| lives(p, s).into_iter().chain(arms(p, s))).chain(gx.union_feeds(p)).chain(gx.run_arms(p)));
        }
    }
    out
}

/// The probe `j` of pipeline `q` on PG's outer-first path that join.inner's step serves (M7 L0-e): an INNER probe (a
/// catalog lookup's too), or a LEFT, SEMI, ANTI or RIGHT SEMI one (every one of them fetches its first outer row before it
/// builds: RIGHT SEMI has no inner fill, M7-DRAIN under M7-PROBE's Owner-Ack).
fn outer_first(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    match gx.pipelines()[q].ops.get(j) {
        Some(Op::Join(JoinDesc::Probe { outer_first: true, jointype, catalog, .. })) => match *jointype {
            JoinType::JOIN_INNER => true,
            JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT_SEMI => !catalog,
            _ => false,
        },
        _ => false,
    }
}

/// Op 14: probe `j` of pipeline `q` on PG's outer-first path (`outer_first`), over a build `b` that may raise and runs
/// alone (no breaker source, no inner probe: its every error is PG's Hash node's): its merge holds the error for the
/// probe's first row. A fed build is held when its feeders' errors can be held with it (V4R-89, rtfilter.rs `held_feeders`).
/// D1906: never a kept build whose side calls (lifetime/hashjoin.rs `kept_road`): its probe takes the lazy build, which runs at
/// the probe's first row and raises there itself, PG's build call; a held error was never raised (a range whose subtype
/// FUNCTION 1 raises, in a kept build's filter: the rows came back without PG's error).
/// The build's own hash keys count (PG's Hash computes each row's as it builds, ExecHashGetHashValue): a key that may
/// raise (an int4 + int8 sum that can overflow) was not held, so it raised before the outer's first row, over an empty
/// outer too, where PG returns rows.
/// D1018 (regress join#346 / #347, probe.rs `nest_held`): a nest inner's probe over a build of the nest's own rerun set
/// whose side may raise (or calls a VOLATILE builtin, whose call count is free; or holds a nest) and whose
/// every node is builtin code is held too, per re-open (probe.rs `nest_holds`).
/// D2756 (P0): never a build over a recursive union `q` does not read (`endless`, D2635): its probe takes the lazy build
/// whatever its side, which runs at the probe's first row and must raise there, PG's MultiExecHash reading the CTE Scan to
/// its end. The merge's DEFER bit held the error the CTE's term raised inside the build's own run, which no probe raises (the
/// probe holds no deferral over a lazy build): the seed's rows came back where PG raises, or a raise at generation 3 crashed.
/// D3273 (D1486): a settled union's emit no rerun set runs, one of whose arms may raise or call (`loud_arms`), is never
/// held as a fed build: op 14 holds the build's own error, not an arm's, so union_arms.rs `hashed` admits only a build
/// that does not defer; its probe takes D824's lazy group with the arms in it, or the recheck licence, as PG's
/// MultiExecHash runs the arms at the outer's first row. A nest inner's probe (`nest_holds`) and a rerun set's build
/// keep their forms.
/// D4051: never a build that calls user code by the effects home's walk (`nonbuiltin_build`: a user function in its keys, a
/// domain whose CHECK is a PL/pgSQL function): run before the outer's first row, it ran where PG, fetching an empty outer
/// first, never builds (the CHECK's NOTICEs), and a user function's raise held past its call left PL/pgSQL's SPI
/// connection pushed (D2044). Such a build's sides read as `UserFn`, as probe.rs `make` reads them: its probe takes D824's
/// lazy build, PG's order, or refuses. A nest inner's probe keeps its form.
pub(crate) fn defers(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> bool {
    let p = &gx.pipelines()[b];
    let user = nonbuiltin_build(gx, q, j);
    let keyed = if user { Side::UserFn } else { own_side_by(gx, b, &|n| side_calls(gx, n), None, usize::MAX) };
    let (own, fed) = if user { (Side::UserFn, Side::UserFn) } else { (own_side(gx, b), side(gx, b)) };
    let unfed = p.desc.consumes.is_empty() && !matches!(p.source, Source::Breaker { .. }) && (own == Side::Raises || keyed == Side::Raises);
    let nest = || q >= gx.pipelines().len() && crate::ops::hashjoin::probe::nest_holds(gx, q, j, b);
    let lazy_endless = || q < gx.pipelines().len() && endless(gx, b, q);
    let held = || crate::ops::hashjoin::strategies::rtfilter::held_feeders(gx, b) && !crate::effects::visible::hashjoin::loud_arms(gx, b);
    outer_first(gx, q, j) && (unfed || ((fed == Side::Raises || fed.max(keyed) == Side::Raises) && held()) || nest()) && !crate::lifetime::hashjoin::kept_road(gx, q, j, b) && !lazy_endless()
}

/// D4051: probe `j` of pipeline `q` reads a Hash whose build calls user code by the effects home's F5 walk over the Hash,
/// its subtree and its hash keys (decompose `PipelineGraph::user_hashes`: a user function, operator or type I/O, or a
/// domain whose CHECK body calls one), which the side class (`side_calls`: a FuncExpr's or OpExpr's function) does not
/// read whole.
pub(crate) fn nonbuiltin_build(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    matches!(gx.pipelines()[q].ops.get(j), Some(Op::Join(JoinDesc::Probe { inner_node, .. })) if gx.graph.user_hashes.contains(inner_node))
}

/// Build `b` of probe `j` of pipeline `q` calls only through one CTE that `q` reads too (a self join of a CTE with a
/// VOLATILE column): `b` and `q` both read that CTE's store, and every other pipeline feeding `b`, `b`'s own ops with its
/// build's keys (D4661: theirs too) and `q`'s ops before the probe are quiet. A CTE computes each row once, in its order,
/// for whichever reader pulls it first; on PG's outer-first path `q` pulls rows until one reaches the probe (then the Hash
/// pulls the rest) or the store ends, so the CTE runs whole in either order and an eager build makes the same calls in
/// the same order.
pub(crate) fn shared_cte(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> bool {
    let cte = |k: usize| match gx.pipelines()[k].source {
        Source::Breaker { slot } => gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&c| c < gx.pipelines().len() && gx.cte_write(c)),
        _ => None,
    };
    let Some(c) = cte(q) else { return false };
    let cp = &gx.pipelines()[c];
    let mut its = feed(gx, c, cp.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = cp.source { Some(slot) } else { None }));
    its.push_in(c);
    let fed = feed(gx, b, gx.pipelines()[b].desc.consumes.iter().copied());
    let uf = |n: Node<'_>| side_calls(gx, n);
    // gen hj-0250: a parameter of an InitPlan whose plan is quiet but for this CTE (every pipeline of it this CTE's or
    // `own_side` Safe): a read runs the plan (PG at the first read, if a row reaches the probe), which raises or calls
    // only what the CTE does, and the CTE runs whole and once in either order
    let ps = gx.pipelines();
    let quiet_plan = |x: usize| {
        x < ps.len() && {
            let src = if let Source::Breaker { slot } = ps[x].source { Some(slot) } else { None };
            feed(gx, x, ps[x].desc.consumes.iter().copied().chain(src)).into_iter().chain([x]).all(|y| its.contains(&y) || own_side(gx, y) == Side::Safe)
        }
    };
    let init = |id: i32| gx.graph.init_plans.iter().any(|i| i.params.iter().any(|p| p == id) && !gx.cte_write(i.pipeline as usize) && quiet_plan(i.pipeline as usize));
    // D4661: `b`'s build keys and its feeding builds' too (`keyed_side`'s reading): the CTE runs whole in either order, but a
    // key runs only where its Hash does, which PG's outer-first join never runs when `q`'s quals leave no row
    cte(b) == Some(c)
        && own_side_by(gx, b, &uf, Some((&init, false)), usize::MAX) == Side::Safe
        && fed.iter().all(|&x| its.contains(&x) || own_side_by(gx, x, &uf, None, usize::MAX) == Side::Safe)
        && own_side_by(gx, q, &uf, None, j) == Side::Safe
}

/// D2635: build `b` reads a recursive union (a pipeline of its side runs a `SetDesc::Iter`) that probe pipeline `q`'s own
/// source does not read: the union may never end (a term that always yields a row), where PG's outer-first hash join never
/// runs the Hash under an empty outer (nodeHashjoin.c HJ_BUILD_HASHTABLE: the first outer fetch returns NULL and the join
/// ends before MultiExecHash). Op 14's deferral and the recheck licence both build eagerly, so such a build takes the lazy
/// build (D824) or refuses. A union `q` streams or sorts itself is read whole by PG on either path (`shared_cte`).
pub(crate) fn endless(gx: &Gx<'_, '_>, b: usize, q: usize) -> bool {
    let ps = gx.pipelines();
    let iters = |k: usize, slots: PgVec<'_, u8>| {
        let runs = |x: &usize| *x < ps.len() && ps[*x].ops.iter().any(|op| crate::graph::step_kind(op) == crate::face::kinds::KindId::SetopRecursive);
        feed(gx, k, slots.into_iter()).into_iter().chain([k]).filter(runs).collect_in(gx.mcx)
    };
    let src = |k: usize| if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let mine = iters(b, ps[b].desc.consumes.iter().copied().chain(src(b)).collect_in(gx.mcx));
    let theirs = iters(q, src(q).into_iter().collect_in(gx.mcx));
    mine.iter().any(|x| !theirs.contains(x))
}

/// Probe `j` of pipeline `q` over build pipeline `b`, on PG's build-first path (D237 (c), D276): an empty build means PG never
/// runs the outer side, i.e. the pipelines feeding `q`'s source, the builds of the probes before `j` and the spools the
/// drains before `j` read (D4195: a lower RIGHT / FULL join's probe pipeline fills its drain's spool). `skip` holds those
/// the runner may skip (DRIVER's `run_empty`): after `b`, below 40 (arg bits 8..47) and read by nothing else. `rest` is the
/// side of the others (they run whatever the build holds), `raising` those of them that may raise; `all` counts every one.
pub(crate) struct Outer<'g> {
    pub skip: u64,
    pub rest: Side,
    pub all: Side,
    pub raising: PgVec<'g, usize>,
}

pub(crate) fn outer<'g>(gx: &Gx<'g, '_>, b: usize, q: usize, j: usize) -> Outer<'g> {
    let p = &gx.pipelines()[q];
    let mut o = Outer { skip: 0, rest: Side::Safe, all: Side::Safe, raising: PgVec::new_in(gx.mcx) };
    // M7 L0-e: every build-first type but those that fill the outer (LEFT, ANTI, FULL: HJ_FILL_OUTER runs it whatever the
    // build holds, nodeHashjoin.c:351)
    let fills = |jt: JoinType| matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL);
    if !matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { outer_first: false, jointype, .. })) if !fills(*jointype)) {
        return o;
    }
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    // D4195: a Drain before `j` hands on the rows of a lower RIGHT / FULL join: its build is `q`'s source, and the pipeline
    // that probes it fills the spool the Drain reads (`matches`); PostgreSQL never runs that lower join over an empty build
    // (nodeHashjoin.c returns before its first outer fetch), so its probe pipeline is outer side too
    let probes = p.ops[..j].iter().filter_map(|op| match op {
        Op::Join(JoinDesc::Probe { slot, .. }) => Some(*slot),
        Op::Join(JoinDesc::Drain { matches, .. }) => Some(*matches),
        _ => None,
    });
    // gen ct-0097: the runner skips a pipeline after the build in every call: one that re-runs in every rerun set the build
    // does (a feeder outside the build's set runs once, ahead of the set's first call: a LATERAL inner's HashAggregate over
    // a CTE, PG's outer of a build that is empty on every call). One re-run where the build is not (D1904's kept table)
    // runs after it in every call
    let calls = |x: usize| gx.graph.reruns.iter().all(|r| !r.pipelines.contains(&(b as u16)) || r.pipelines.contains(&(x as u16)));
    // M7 L0-b2: a source that may raise or call (a VALUES row, a function, an index key) is `q`'s own and runs
    // only after the build, so an empty build lets the runner skip `q` whole: PG never fetches its outer
    // a nest inner's own source the runner cannot skip, read quietly with the bound parameters (`own_side_by`: an initdb
    // btree index of quiet keys, regress create_table_like#464's pg_attribute walk), raises on no input
    let own_quiet = || matches!(own_side_by(gx, q, &|n| side_calls(gx, n), Some((&|id| gx.graph.params.iter().any(|x| x.0 == id), quiet_statement(gx))), 0), Side::Safe);
    if src.is_none() && !matches!(p.source, Source::Range { .. }) {
        o.all = Side::Raises;
        match q < 40 && calls(q) {
            true => o.skip |= 1 << q,
            false if q >= gx.pipelines().len() && own_quiet() => {}
            false => {
                o.rest = Side::Raises;
                o.raising.push_in(q);
            }
        }
    }
    // gen ct-0097: a `q` reading a CTE's store as its source pulls the CTE as it reads (ops/ctescan/lower.rs): the runner
    // skips it whole over an empty build (DRIVER's `run_empty`, a site-less pipeline's too), so it pulls no row
    if q > b && q < 40 && calls(q) && src.and_then(|s| gx.life_of(q, s)).is_some_and(|l| (l.0 as usize) < gx.pipelines().len() && gx.cte_write(l.0 as usize)) {
        o.skip |= 1 << q;
    }
    let f = feed(gx, q, src.into_iter().chain(probes));
    let reads = |c: usize, x: usize| gx.pipelines()[c].desc.consumes.iter().any(|&s| gx.life_of(c, s).is_some_and(|l| l.0 as usize == x));
    // gen dsx-0124 / shop-0445 (held-out M4): a pipeline that runs whatever the build holds counts what PG 18.6 would raise
    // there, `side_by`'s reading (W4b-RowX): an initdb btree index or bitmap of quiet keys (a Hash Right Join's build over a
    // date range) is quiet, as a heap scan is; a statement with a `loud` pipeline reads as before
    let (bound, quiet) = (|id: i32| gx.graph.params.iter().any(|x| x.0 == id), quiet_statement(gx));
    let uf = |n: Node<'_>| side_calls(gx, n);
    let ran = |x: usize| {
        let ops = &gx.pipelines()[x].ops;
        own_side_by(gx, x, &uf, Some((&bound, quiet)), ops.len() - matches!(ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) as usize)
    };
    let mut kept = PgVec::new_in(gx.mcx);
    for &x in &f {
        let s = own_side(gx, x);
        o.all = o.all.max(s);
        let only = (0..gx.pipelines().len()).all(|c| c == q || f.contains(&c) || !reads(c, x));
        if x > b && x < 40 && only && calls(x) {
            o.skip |= 1 << x;
        } else {
            kept.push_in((x, ran(x)));
        }
    }
    // gen ct-0097: a CTE some reader still pulls over an empty build runs its pulled set whole at that pull, the mask or
    // not (DRIVER `pull_nested`): none of it is skipped, and each counts (to the fixpoint: an unmasked member may be
    // another CTE's reader)
    let unpulled = loop {
        let unpulled = pulled_by_skipped(gx, &f, o.skip);
        let mut more = false;
        for &c in f.iter().filter(|&&c| gx.cte_write(c) && !unpulled.contains(&c)) {
            let skip = o.skip;
            for x in pulled_set(gx, c).into_iter().filter(|&x| x < 40 && skip >> x & 1 != 0) {
                o.skip &= !(1 << x);
                kept.push_in((x, ran(x)));
                more = true;
            }
        }
        if !more {
            break unpulled;
        }
    };
    let whole = whole_at_build(gx, b);
    for (x, s) in kept.into_iter().filter(|(x, _)| !whole.contains(x) && !unpulled.contains(x)) {
        o.rest = o.rest.max(s);
        if s == Side::Raises {
            o.raising.push_in(x);
        }
    }
    o
}

/// D3373: pipeline `k` is in a CTE's pulled set (`pulled_set`).
pub(crate) fn in_pulled_set(gx: &Gx<'_, '_>, k: usize) -> bool {
    (0..gx.pipelines().len()).filter(|&c| gx.cte_write(c)).any(|c| pulled_set(gx, c).contains(&k))
}

/// A CTE's pulled set: its producer `c` and every pipeline feeding it (ops/ctescan/lower.rs: they run at its readers' pulls).
fn pulled_set<'g>(gx: &Gx<'g, '_>, c: usize) -> PgVec<'g, usize> {
    let cp = &gx.pipelines()[c];
    let mut its = feed(gx, c, cp.desc.consumes.iter().copied().chain(if let Source::Breaker { slot } = cp.source { Some(slot) } else { None }));
    its.push_in(c);
    its
}

/// Gen ct-0097: the pulled sets of the CTEs in `f` whose every reader is a pipeline the runner skips over an empty build
/// (`skip`): no reader pulls the CTE then, so it never runs (ops/ctescan/lower.rs), as PG's outer, never fetched, never
/// reads its CTE Scan. Not a data-modifying CTE (drained at the statement's end), one a rerun set runs (D1385's eager
/// form, or a set's member: the set runs it per call; not a set wholly inside the pulled set, a recursive union's
/// generations) nor one an embedded inner reads (no pipeline the mask names).
fn pulled_by_skipped<'g>(gx: &Gx<'g, '_>, f: &[usize], skip: u64) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let reads = |p: usize, x: usize| {
        let src = if let Source::Breaker { slot } = ps[p].source { Some(slot) } else { None };
        ps[p].desc.consumes.iter().copied().chain(src).any(|s| gx.life_of(p, s).is_some_and(|l| l.0 as usize == x))
    };
    let mut out = PgVec::new_in(gx.mcx);
    for &c in f.iter().filter(|&&c| gx.cte_write(c)) {
        let set = pulled_set(gx, c);
        let writes = set.iter().any(|&p| ps[p].ops.iter().any(|op| matches!(op, Op::Write(_))));
        // a set inside the pulled set (a recursive union's generations, setop P6) runs only at the CTE's pulls too
        let rerun = gx.graph.reruns.iter().any(|r| set.iter().any(|&p| r.pipelines.contains(&(p as u16))) && !r.pipelines.iter().all(|&x| set.contains(&(x as usize))));
        let inner = ps.inners().iter().any(|(_, x)| x.desc.consumes_from.iter().any(|l| l.0 as usize == c));
        let readers = (0..ps.len()).filter(|&p| !set.contains(&p) && reads(p, c)).collect_in(gx.mcx);
        if !writes && !rerun && !inner && !readers.is_empty() && readers.iter().all(|&p| p < 40 && skip & 1 << p != 0) {
            out.extend_in(set);
        }
    }
    out
}

/// The build-first twin of `shared_cte` (regress window#343): the pipelines build `b` runs whole before PG runs the outer
/// side, whatever the table then holds. `b` reads a CTE's store as its source through filters and projections alone, its
/// ops and keys quiet: MultiExecHash pulls its CTE Scan to the end, and a CTE computes each row once, in its order, for
/// whichever reader pulls it first, so the CTE's producer and its feeders make every call and raise every error at the
/// build, and an outer side reading the same store adds none. A gate or a probe in `b` may stop it early: none (a LIMIT: below).
/// Gen ct-0086: so does a `b` over one breaker whose producer reads the store so (a HashAggregate of an IN list's keys, a
/// Sort) and folds no aggregate: the breaker takes its input whole before `b` reads a row.
/// Regress with#112: and a `b` that is a settled union's emit (material's rerun/union_build.rs admits it only where
/// MultiExecHash runs every arm to its end at the build), its other ops filters, projections and the build, quiet: each
/// feed reading a CTE's store through filters and projections reads it whole there, so the CTE's pulled set runs whole at
/// the build. A feed's own raise can stop PG's pull early, but then both sides raise (an error
/// against an error passes); neither the feed nor the CTE's pulled set calls a user or VOLATILE function, so no call differs.
/// Or a `b` that reads the store through a constant LIMIT of one row or more (no WITH TIES), filters and projections
/// before it and projections alone after: the build is empty only when the store ran out first (every row computed at the
/// build), and otherwise PG runs the outer side, whose reads of the store compute the rest in the CTE's own order: the CTE
/// makes the same calls and raises the same errors whichever side pulls a row first.
fn whole_at_build<'g>(gx: &Gx<'g, '_>, b: usize) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let uf = |n: Node<'_>| side_calls(gx, n);
    let rows = |x: usize| ps[x].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Build { .. }))) && own_side_by(gx, x, &uf, None, ps[x].ops.len()) == Side::Safe;
    let capped = |x: usize| {
        use crate::frame::iface::{limit_settled, limit_stopping, limit_with_ties};
        let ops = &ps[x].ops;
        let cap = |op: &Op<'_>| limit_settled(op) && limit_stopping(op).is_none() && !limit_with_ties(op);
        let Some(j) = ops.iter().position(cap) else { return false };
        let before = ops[..j].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
        let after = ops[j + 1..].iter().all(|op| matches!(op, Op::Project(_) | Op::Join(JoinDesc::Build { .. })));
        before && after && own_side_nests(gx, x, &uf, None, ops.len(), true) == Side::Safe
    };
    let from = |x: usize| match ps[x].source {
        Source::Breaker { slot } => gx.life_of(x, slot).map(|l| l.0 as usize).filter(|&y| y < ps.len()),
        _ => None,
    };
    if crate::rerun::union_arms::union_reader(&ps[b]) {
        let quiet = ps[b].ops[1..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Build { .. }))) && own_side_by(gx, b, &uf, None, ps[b].ops.len()) == Side::Safe;
        let calls = |x: usize| own_side_by(gx, x, &uf, None, ps[x].ops.len()) == Side::UserFn;
        let feed = |f: usize| ps[f].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)) || crate::rerun::union_arms::feed_op(op));
        let mut out = PgVec::new_in(gx.mcx);
        for f in gx.union_feeds(b).into_iter().filter(|&f| quiet && feed(f) && !calls(f)) {
            if let Some(set) = from(f).filter(|&c| gx.cte_write(c)).map(|c| pulled_set(gx, c)).filter(|set| !set.iter().any(|&x| calls(x))) {
                out.extend_in(set);
            }
        }
        return out;
    }
    let c = match from(b) {
        Some(c) if (rows(b) || capped(b)) && gx.cte_write(c) => c,
        Some(x) if rows(b) && rows(x) && gx.group_at(x).is_none_or(|g| g.aggs.is_empty()) => match from(x) {
            Some(c) if gx.cte_write(c) => c,
            _ => return PgVec::new_in(gx.mcx),
        },
        _ => return PgVec::new_in(gx.mcx),
    };
    pulled_set(gx, c)
}

/// D3014 build-once: build `b` is fed, its pipeline reads a slot (a breaker source, a probe of another build) whose
/// producers run before it.
pub(crate) fn fed(gx: &Gx<'_, '_>, b: usize) -> bool {
    let p = &gx.pipelines()[b];
    !p.desc.consumes.is_empty() || matches!(p.source, Source::Breaker { .. })
}

/// D824 (V4R-298): build `b` of probe `j` of pipeline `q` (PG's outer-first hash join, INNER / LEFT / SEMI / ANTI by M7
/// L0-e, a user function on the build side, no op 14 deferral) runs lazily: skipped in pipeline order, its slot open
/// and pending, run when the first row reaches the probe (the driver's `BuildWait`), as PG's HJ_BUILD_HASHTABLE does.
/// D1005: a fed `b`'s producers (`feed`: its breaker source's and its inner probes' builds, transitively) are skipped
/// and run with it, first, in order: PG runs the Hash's subtree only then. The pipelines to defer (the producers
/// ascending, `b` last), only where running them later reorders nothing PG would see: `q` runs on the leader (frame.rs
/// `Mechs::shaped` pins it, as a pool-unsafe host site pins its pipeline), `b` read by `q` alone and each producer by
/// the deferred ones alone, no pass after any, none lazy for another probe, and every other pipeline from the first
/// deferred one to `q` quiet (or an earlier probe's build whose error op 14 holds: `held_before`; D3601: or the set of
/// a hashed SubPlan step before the probe, `set_before`); consumed index < 8, slot < 64. A producer already lazy for
/// one of them (an inner outer-first join's build, its probe among them) stays out: it waits for that probe in turn, as
/// PG's inner Hash Join builds only at its own first outer row (nested waits, `Node::hold` stacks). D3379 FAMCLAIM
/// HASH: or the arms of one union that each probe `b` (an UPDATE / DELETE over a partitioned target whose every leaf's
/// scan probes the hashed subquery: PostgreSQL's one Hash Join over the target's Append): the first arm declares the
/// group, read by every arm (`lazy_group_by`), and a later arm waits on the pending slot when no earlier arm ran it
/// (driver entry.rs `lazy_build`: a later reader of a lazy build's slot), as PostgreSQL builds the Hash at the first
/// row its outer Append hands on, whichever leaf makes it.
pub(crate) fn lazy<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, j: usize, consumed: usize, slot: u8) -> Option<PgVec<'g, usize>> {
    let probe = matches!(px.gx.pipelines()[q].ops.get(j), Some(Op::Join(JoinDesc::Probe { catalog: false, .. })));
    if !(probe && outer_first(px.gx, q, j)) {
        return None;
    }
    if let Some(arms) = probe_arms(px.gx, b, q) {
        if arms[0] != q {
            return px.lazy.iter().any(|&(x, r)| x == b && r == arms[0]).then(|| PgVec::new_in(px.mcx()));
        }
        return lazy_group_by(px, b, q, &arms[..], consumed, slot, &[]);
    }
    lazy_group(px, b, q, consumed, slot, &[])
}

/// D3379 FAMCLAIM HASH (`lazy`): the pipelines that read build `b`'s product, ascending, when there are two or more, `q`
/// among them, each one a union's arm (an Append arm marker) probing it on PG's outer-first path; else `None`.
fn probe_arms<'g>(gx: &Gx<'g, '_>, b: usize, q: usize) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let s = ps[b].desc.produces()?;
    let readers = (b + 1..ps.len()).filter(|&r| ps[r].desc.consumes.contains(&s) && gx.life_of(r, s).is_some_and(|l| l.0 as usize == b)).collect_in(gx.mcx);
    let probes = |r: usize| ps[r].ops.iter().enumerate().any(|(jr, o)| matches!(o, Op::Join(JoinDesc::Probe { slot, catalog: false, .. }) if *slot == s) && outer_first(gx, r, jr));
    let arm = |r: usize| ps[r].ops.iter().any(crate::graph::is_arm);
    (readers.len() >= 2 && readers.contains(&q) && readers.iter().all(|&r| probes(r) && arm(r))).then_some(readers)
}

/// D2717: `lazy` for probe `j` of `q`; in the spill pair pass's copy of it (spill/pair.rs `pair`, `JoinExt::pairing`) the
/// original's reading: the original declared build `b`'s group (`px.lazy`), so the copy takes it as lazy and declares none
/// (`lazy_group` would refuse a group already declared). The pass runs after `q`, so `b` has run before any kept row.
pub(crate) fn lazy_or_copy<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, j: usize, consumed: usize, slot: u8) -> Option<PgVec<'g, usize>> {
    let pairing = px.ext::<crate::ops::hashjoin::build_side::JoinExt>(crate::face::kinds::Face::Join).and_then(|e| e.pairing);
    if pairing == Some((q, px.ident(q))) && px.lazy.contains(&(b, q)) {
        return Some(PgVec::new_in(px.mcx()));
    }
    lazy(px, b, q, j, consumed, slot)
}

/// D824's group and admission for build `b` read by pipeline `q` at its first row (`lazy`'s probe; merge.join's inner at
/// INITIALIZE_INNER, MERGE R3 (ii), whose outer's feeders `before` PG runs first: not quiet, not in the group, W8-D1236).
pub(crate) fn lazy_group<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, consumed: usize, slot: u8, before: &[usize]) -> Option<PgVec<'g, usize>> {
    lazy_group_by(px, b, q, &[q], consumed, slot, before)
}

/// `lazy_group` with `readers` the pipelines that may read `b`, `q` the first (rowsecurity#343: a streamed write's arms
/// whose nest copies read one Material, material's rerun/spool/lazy.rs).
pub(crate) fn lazy_group_by<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, readers: &[usize], consumed: usize, slot: u8, before: &[usize]) -> Option<PgVec<'g, usize>> {
    lazy_group_with(px, b, q, readers, (consumed, slot), before, false)
}

/// D3897 (material's rerun/spool/lazy.rs `drain_wait`): `lazy_group` for the probe `b` that fills a RIGHT / FULL drain's
/// spool nest `q`'s embedded inner reads: the build it probes is read by `q` too, through that inner's drain alone.
pub(crate) fn lazy_group_drain<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, consumed: usize, slot: u8, before: &[usize]) -> Option<PgVec<'g, usize>> {
    lazy_group_with(px, b, q, &[q], (consumed, slot), before, true)
}

/// `lazy_group_by`; `inner`: a producer of `b` may be read by `readers` too (`lazy_group_drain`).
fn lazy_group_with<'g>(px: &crate::frame::px::Px<'g, '_>, b: usize, q: usize, readers: &[usize], (consumed, slot): (usize, u8), before: &[usize], inner: bool) -> Option<PgVec<'g, usize>> {
    let gx = px.gx;
    let bp = &gx.pipelines()[b];
    let reads = |c: usize, x: usize| gx.pipelines()[c].desc.consumes.iter().any(|&s| gx.life_of(c, s).is_some_and(|l| l.0 as usize == x));
    let src = if let Source::Breaker { slot } = bp.source { Some(slot) } else { None };
    let mut all = feed(gx, b, bp.desc.consumes.iter().copied().chain(src));
    // D2340: a CTE's pulled set (its plan and its producers) runs only at its readers' pulls, never in this group: run here
    // too, the reader's pull ran its producers a second time over freed slots
    // D4189: the set `q` itself is in (the CTE's own plan, a recursive union's term among it) is no such reader's: `b`'s
    // feeders there are that pull's producers, which it ran at its first call, ahead of the outer's first row (a hashed
    // aggregate, a hashed DISTINCT or a top-k under the Hash calling a function that raises, over an outer that hands no
    // row: PostgreSQL never builds and returns its rows, the pull raised). They stay in the group, which the pull steps
    // past (driver `Entry::pull_nested`'s lazy members; exec ops/ctescan/proto.rs `members` for a term's probe) and the
    // probe's wait runs
    let pulled = (0..gx.pipelines().len()).filter(|&c| gx.cte_write(c)).flat_map(|c| {
        let cp = &gx.pipelines()[c];
        let csrc = if let Source::Breaker { slot } = cp.source { Some(slot) } else { None };
        let mut v = feed(gx, c, cp.desc.consumes.iter().copied().chain(csrc));
        v.push_in(c);
        if v.contains(&q) { PgVec::new_in(gx.mcx) } else { v }
    }).collect_in(gx.mcx);
    all.retain(|x| !pulled.contains(x));
    all.push_in(b);
    // nested: a producer already lazy for one of these (an inner outer-first join's build) waits for that probe in turn
    let deep = |x: usize| px.lazy.iter().any(|&(y, z)| y == x && all.contains(&z));
    let mut group: PgVec<'g, usize> = all.iter().copied().filter(|&x| !deep(x) && !before.contains(&x)).collect_in(gx.mcx);
    group.sort_unstable();
    let only = |x: usize, by: &[usize]| (0..gx.pipelines().len()).all(|c| by.contains(&c) || !reads(c, x));
    // D3374: the lazy build's probe in `q` (none for merge.join's inner): a pipeline between that is an earlier probe's
    // held build (`held_before`) raises only at that probe's first row, which every row reaching this one passes first
    let jb = gx.pipelines()[q].ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == slot));
    // D3749: a later nest's Material fill that waits for that nest's first outer row (rerun/spool/lazy.rs `waits_after`,
    // River's conflicts read from the window CTE this build hashes) runs after this build, not between
    let later = jb.map_or_else(|| PgVec::new_in(gx.mcx), |jb| crate::rerun::spool::lazy::waits_after(gx, q, jb));
    let held = |x: usize| jb.is_some_and(|jb| only(x, &[q]) && held_before(gx, x, q, jb));
    // D3601: or the set build of a hashed SubPlan step `q` runs on every row before that probe (`set_before`)
    let set = |x: usize| jb.is_some_and(|jb| only(x, &[q]) && set_before(gx, x, q, jb));
    // D3745: or a rerun set's member that `q` alone runs (`owned_by`): it runs only at its owner's call, inside `q`'s run,
    // never ahead of the rows reaching `q`'s probe
    let owned = |x: usize| owned_by(gx, x, q);
    // D3788: or a producer of `q`'s source whose fill ends before its first row (`src_filled`): PostgreSQL runs it
    // before `q`'s first row, so before the lazy build; or, in a statement that writes nothing, a pipeline that may
    // raise but acts nowhere (`raise_only`): run before the build where PostgreSQL runs it after, only which error is
    // raised differs. Neither for a group a rerun set runs: a lazy build there is refused (rerun/sets.rs, D1654) where
    // the eager build runs ahead under the unchecked licence (`Px::ahead_unchecked`) and raises as PostgreSQL does
    let rerun = group.iter().any(|&g| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(g as u16))));
    let filled = if rerun { PgVec::new_in(gx.mcx) } else { src_filled(gx, q) };
    let quiet = !rerun && group.iter().all(|&g| keyed_side(gx, g) != Side::UserFn && !crate::effects::own_hides_user_code(gx, g));
    let raises = |x: usize| quiet && raise_only(gx, x).is_some();
    let mut raised = PgVec::new_in(gx.mcx);
    let ok = b < q
        && consumed < 8
        && slot < 64
        && group.last() == Some(&b)
        && group.iter().all(|&x| !px.pass_after.contains(&x))
        && only(b, readers)
        && group[..group.len() - 1].iter().all(|&x| only(x, &all) || inner && only(x, &mcx::concat_in(&[&all[..], readers], gx.mcx)[..]))
        && !px.lazy.iter().any(|&(x, _)| group.contains(&x) || x == q)
        && (group[0] + 1..q).filter(|&c| !all.contains(&c) && !before.contains(&c) && !later.contains(&c)).all(|x| {
            own_side(gx, x) == Side::Safe
                || between_safe(gx, x) && (fires::fire("lazy.between.nest"), true).1
                || held(x) && (fires::fire("lazy.between.held"), true).1
                || set(x) && (fires::fire("lazy.between.set"), true).1
                || owned(x) && (fires::fire("lazy.between.owned"), true).1
                || filled.contains(&x) && (fires::fire("lazy.between.filled"), true).1
                || raises(x) && {
                    fires::fire("lazy.between.raises");
                    raised.push_in(x);
                    true
                }
        });
    // `raise_only`'s pipelines run ahead of PostgreSQL's order unchecked: emit tags them once the group is declared
    if ok {
        px.lazy_raises.borrow_mut().extend(raised.into_iter().map(|x| (b, q, x))); // alloc: Program
    }
    ok.then_some(group)
}

/// D3745 (pganalyze's relation stats: a LEFT join's build over a CTE that calls pg_lock_status(), with correlated
/// SubPlans in the target list or LATERAL aggregates above the join): pipeline `x`, between a lazy group and its reader
/// `q` in program order, is a member of rerun sets each of which `q` runs (`rerun::sets::set_owner`: a correlated
/// SubPlan's plan, a nest's inner). A member runs only through its owner's m8_rerun (frame/program.rs), at the call a
/// row reaching the owner step makes inside `q`'s run, as PostgreSQL runs the plan at that row's call: it never runs
/// ahead of the rows reaching `q`'s probe, so it keeps PostgreSQL's order with the lazy build whatever it raises or
/// calls (an owner step before the probe runs it for the rows PostgreSQL's outer fetch passes first; one after it, for
/// rows the probe returned, after the build).
fn owned_by(gx: &Gx<'_, '_>, x: usize, q: usize) -> bool {
    let sets = (0..gx.graph.reruns.len()).filter(|&s| gx.graph.reruns[s].pipelines.contains(&(x as u16))).collect_in(gx.mcx);
    !sets.is_empty() && sets.iter().all(|&s| crate::rerun::sets::set_owner(gx, s) == Some(q))
}

/// D3788 (pgwatch's table stats: a NOT EXISTS over pg_locks on an anti join whose outer is a chain of RIGHT joins
/// inside a sorted CTE): the pipelines that fill reader `q`'s source, transitively (`feed`), when its breaker hands on
/// its first row only once its fill has ended: a sort or a top-N, a hashed or plain aggregate, a hash table a join's
/// drain reads (PostgreSQL's Hash runs whole before the join's first row). Under a Materialize or a CTE's store, which
/// PostgreSQL streams (it runs the rest of their fill after the first row, so after a lazy build of a probe in `q`),
/// the fill itself never, but what fills its own source by the same rule. PostgreSQL runs each of them, as far as it
/// runs it at all, before `q`'s first row, so before that build (built at the first row reaching its probe): the
/// program's order between them and the build is PostgreSQL's, whatever they raise or call. Whether one runs where
/// PostgreSQL never does (an empty outer below it) is its own family's question, asked where it is made.
fn src_filled<'g>(gx: &Gx<'g, '_>, q: usize) -> PgVec<'g, usize> {
    crate::stack_check();
    use gravity_abi::breaker::BreakerKind;
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[q].source else { return PgVec::new_in(gx.mcx) };
    let Some(f) = gx.life_of(q, slot).map(|l| l.0 as usize).filter(|&f| f < q) else { return PgVec::new_in(gx.mcx) };
    match crate::frame::iface::breaker_of(&ps[f].desc).filter(|b| b.slot == slot).map(|b| b.kind) {
        Some(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::HashAgg | BreakerKind::PlainAgg | BreakerKind::HashBuild) => feed(gx, q, core::iter::once(slot)),
        Some(BreakerKind::Materialize) => src_filled(gx, f),
        _ => PgVec::new_in(gx.mcx),
    }
}

/// D3788 (pgwatch's table stats, `DECISIONS.md` 4 (D) b: a later UNION ALL arm's join that may raise, run before the
/// first arm's lazy anti-join build): pipeline `x`, run in program order between a lazy group and its reader, may raise
/// but acts nowhere: its own side calls no user or VOLATILE function and locks no row (`keyed_side` Raises), it holds no
/// such call that side does not read (`effects::own_hides_user_code`), and it hashes no key whose collation raises 42P22
/// (D2849, not a data error); the group calls no user or VOLATILE function either (`lazy_group_by`'s `quiet`). `x` runs
/// ahead of PostgreSQL's order (PostgreSQL runs it after the reader's arm), so its error may abort the statement before
/// acting calls PostgreSQL makes first elsewhere (a nextval, a VOLATILE reader in the reader's arm): as every unchecked
/// departure it asks `licence_unchecked`, which grants it only in a statement that acts nowhere (a SELECT that writes,
/// locks and calls nothing VOLATILE; a write keeps PostgreSQL's order, D3540), and its errors carry `Px::ahead_unchecked`'s
/// tag (`Px::lazy_raises`, set by emit once the group is declared; denied where EXPLAIN has no name for its node), so the
/// harness reads the error raised in place of the build's or of PostgreSQL's rows against the data classes (21 / 22 / 23).
/// `x` runs where it runs whether or not the build is lazy; only the build moves past it. Construct C2: the tagged licence
/// `x` runs ahead under, or `None`.
fn raise_only(gx: &Gx<'_, '_>, x: usize) -> Option<gravity_decompose::effects::Licence> {
    use gravity_decompose::effects::{licence_unchecked, Effects};
    use types_nodes::nodes_enums::CmdType;
    let select = gx.graph.stmt.is_some_and(|s| s.commandType == CmdType::CMD_SELECT && s.rowMarks.iter().next().is_none() && !s.hasModifyingCTE);
    if !(select && keyed_side(gx, x) == Side::Raises && !crate::effects::own_hides_user_code(gx, x) && !crate::effects::hashes_raising_key(gx, x)) {
        return None;
    }
    let l = licence_unchecked(Effects::RAISES, false).ok().filter(|l| l.tagged())?;
    crate::frame::px::ahead_tag(gx, x).is_some().then_some(l)
}

/// D3601 (River's completer: a CTE UPDATE whose target scan's NOT IN probes the set of another CTE UPDATE's store
/// before the scan probes the hashed claim CTE): pipeline `x`, run in program order between a lazy group and its reader
/// `q`, ends in the build of a hashed SubPlan whose step in `q` comes before the lazy build's probe `jb` and runs on
/// every row there (a row site, never conditional). PostgreSQL builds that set at the first row reaching the step
/// (ExecHashSubPlan, buildSubPlanHash), and every row reaching `jb` passed the step first, so on either path the set is
/// built before the Hash runs: running the group after `x` keeps PostgreSQL's order between them, whatever `x` raises
/// or does.
fn set_before(gx: &Gx<'_, '_>, x: usize, q: usize, jb: usize) -> bool {
    use gravity_decompose::ops::shared::subplan::{SubForm, SubSite};
    let ps = gx.pipelines();
    let (Some(Op::SubPlan(d)), Some(s)) = (ps[x].ops.last(), ps[x].desc.produces()) else { return false };
    let step = |o: &Op<'_>| matches!(o, Op::SubPlan(e) if e.plan_id == d.plan_id && matches!(e.form, SubForm::Hashed { slot } if slot == s) && e.site == SubSite::Row && !e.conditional);
    matches!(d.form, SubForm::Build) && gx.life_of(q, s).is_some_and(|l| l.0 as usize == x) && ps[q].ops[..jb].iter().any(step)
}

/// D3374 (hashjoin_cte_self_join_raise's earlier probe build): pipeline `x`, run in program order between a lazy group
/// and its reader `q`, is an unfed build (no breaker source, nothing consumed) of probe `jx` of `q`, earlier than the
/// lazy build's probe `jb`, whose error op 14 holds for `jx`'s first row (`defers`), and whose side, its keys too,
/// raises and calls nothing (no user or VOLATILE call: run ahead it makes no call PG would not). Its error raises at
/// `jx`'s first row, which every row reaching `jb` passes first, as PG's lower outer-first Hash Join builds at its first
/// outer row before its parent's Hash runs; with no row at `jx`, the held error is dropped and PG never builds.
fn held_before(gx: &Gx<'_, '_>, x: usize, q: usize, jb: usize) -> bool {
    let p = &gx.pipelines()[x];
    let unfed = p.desc.consumes.is_empty() && !matches!(p.source, Source::Breaker { .. });
    let keyed = own_side_by(gx, x, &|n| side_calls(gx, n), None, usize::MAX);
    unfed && keyed == Side::Raises && gx.pipelines()[q].ops[..jb].iter().enumerate().any(|(jx, o)| {
        matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if gx.life_of(q, *slot).is_some_and(|l| l.0 as usize == x)) && defers(gx, x, q, jx)
    })
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::{Am, NoFacts, PipelineGraph, PipelineNode};
    use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
    use gravity_expr::catalog::{DomainFacts, FuncFacts, IoFacts, Support, TextFacts, TypeFacts};
    use gravity_expr::PgCatalog;
    use gravity_fn::{Shim, Tier};
    use gravity_program::{Arena, ArenaVec, End, Pipeline, Reason, SiteDesc};
    use types_core::Oid;
    use types_error::PgResult;
    use types_nodes::list::NodeList;

    use super::*;

    /// D237 (c) / D276: on the build-first path the outer side is the pipelines that fed the probe's source (a breaker's
    /// producer; a scan is quiet, a VALUES source may raise). The runner may skip a producer only when it runs after the
    /// build: one that ran before counts whatever the build holds.
    #[test]
    fn the_outer_side_is_the_probe_sources_producers_and_skips_only_those_after_the_build() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for (quiet, build_first) in [(true, true), (false, true), (false, false)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
            let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let build = PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0));
            let producer = PipelineNode::new(a, 2, if quiet { scan() } else { Source::Values { scanrelid: 2, lists: nil } }, into(BreakerKind::HashAgg, 1));
            let mut g = PipelineGraph::new(a);
            let (b, x) = if build_first { (0, 1) } else { (1, 0) };
            let [first, second] = if build_first { [build, producer] } else { [producer, build] };
            g.pipelines.push(first);
            g.pipelines.push(second);
            let mut p = PipelineNode::new(a, 3, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(side(&gx, b), Side::Safe, "a scan's build is quiet");
            let o = outer(&gx, b, 2, 0);
            let s = if quiet { Side::Safe } else { Side::Raises };
            assert_eq!((o.all, o.skip), (s, if build_first { 1 << x } else { 0 }), "quiet {quiet}, build first {build_first}");
            assert_eq!(o.rest, if build_first { Side::Safe } else { s });
        }
    }

    /// window#343: a build-first probe's outer side fed by a CTE (p0, a VALUES that may raise) the build reads whole as its
    /// source (p1) leaves the CTE out of `rest`: the build runs it whole first, or through a constant LIMIT of one row or
    /// more (empty only when the CTE ran out). Not a CTE (no Cte InitPlan), or a LIMIT 0 in the build (it reads nothing):
    /// the CTE counts in `rest` as before.
    #[test]
    fn a_cte_the_build_reads_whole_is_not_the_outer_sides() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::{Bound, InitPlan};
        use gravity_decompose::ops::shared::subplan::SubLink;
        use types_nodes::list::IntList;
        for (cte, limit, want) in [(true, None, Side::Safe), (false, None, Side::Raises), (true, Some(5), Side::Safe), (true, Some(0), Side::Raises)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(PipelineNode::new(a, 1, Source::Values { scanrelid: 1, lists: nil }, into(BreakerKind::Materialize, 0)));
            let mut b = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, into(BreakerKind::HashBuild, 1).consuming_one(0));
            if let Some(n) = limit {
                b.push(Op::Limit { count: Bound::Const(n), offset: Bound::All, ties: None });
            }
            b.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX }));
            g.pipelines.push(b);
            g.pipelines.push(PipelineNode::new(a, 3, Source::Breaker { slot: 0 }, into(BreakerKind::Materialize, 2).consuming_one(0)));
            let mut q = PipelineNode::new(a, 4, Source::Breaker { slot: 2 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(2));
            q.push(Op::Join(JoinDesc::Probe { slot: 1, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 2, outer_first: false, catalog: false }));
            g.pipelines.push(q);
            if cte {
                let params: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
                g.init_plans.push(InitPlan { plan_id: 1, pipeline: 0, link: SubLink::Cte, param: -1, params, slot: Some(0), volatility: Volatility::Immutable, raise_free: false, parallel_safe: true, attach: -1 });
            }
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let o = outer(&gx, 1, 3, 0);
            assert_eq!((o.rest, o.all, o.skip), (want, Side::Raises, 1 << 2), "cte {cte}, limit {limit:?}");
        }
    }

    /// gen ct-0097: a CTE (p0, a VALUES that may raise) only pipelines the runner skips over an empty build read is pulled by
    /// nobody then: out of `rest`. Through a HashAggregate (p2, after the scan build p1, skipped as a feeder) or read by the
    /// probe pipeline itself as its source (p2: flagged as the CTE's reader). Not a CTE (an eager producer before the
    /// build): in `rest`, and a probe pipeline over its slot is not flagged. A rerun set holding the build and the probe but
    /// not the HashAggregate (a LATERAL inner: the HashAggregate runs once, before the set's first call): nothing skipped,
    /// the CTE in `rest`. A rerun set inside the CTE's pulled set (a recursive union's generations: setop P6's union as the
    /// probe side over an empty build) runs only at its pulls: out of `rest` as without it.
    #[test]
    fn a_cte_only_skipped_outer_pipelines_pull_is_not_the_outer_sides() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::{InitPlan, Rerun};
        use gravity_decompose::ops::shared::subplan::SubLink;
        use types_nodes::list::IntList;
        let cases = [(true, true, 0, Side::Safe, 1 << 2), (false, true, 0, Side::Raises, 1 << 2), (true, false, 0, Side::Safe, 1 << 2), (false, false, 0, Side::Raises, 0), (true, true, 1, Side::Raises, 0), (true, true, 2, Side::Safe, 1 << 2)];
        for (cte, agg, rerun, want, skip) in cases {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let scan = Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(PipelineNode::new(a, 1, Source::Values { scanrelid: 1, lists: nil }, into(BreakerKind::Materialize, 0)));
            let mut b = PipelineNode::new(a, 2, scan, into(BreakerKind::HashBuild, 1));
            b.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX }));
            g.pipelines.push(b);
            if agg {
                g.pipelines.push(PipelineNode::new(a, 3, Source::Breaker { slot: 0 }, into(BreakerKind::HashAgg, 2).consuming_one(0)));
            }
            let s = if agg { 2 } else { 0 };
            let mut q = PipelineNode::new(a, 4, Source::Breaker { slot: s }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(s));
            q.push(Op::Join(JoinDesc::Probe { slot: 1, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 2, outer_first: false, catalog: false }));
            g.pipelines.push(q);
            if cte {
                let params: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
                g.init_plans.push(InitPlan { plan_id: 1, pipeline: 0, link: SubLink::Cte, param: -1, params, slot: Some(0), volatility: Volatility::Immutable, raise_free: false, parallel_safe: true, attach: -1 });
            }
            if rerun != 0 {
                let mut pipelines = ArenaVec::new_in(a);
                pipelines.extend_from_slice(if rerun == 1 { &[1u16, 3][..] } else { &[0u16][..] });
                g.reruns.push(Rerun { node_id: 9, plan_id: 0, pipelines, slots: [0u64; 4] });
            }
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let o = outer(&gx, 1, 2 + agg as usize, 0);
            assert_eq!((o.rest, o.all, o.skip), (want, Side::Raises, skip), "cte {cte}, agg {agg}, rerun {rerun}");
        }
    }

    /// gen ct-0086: a build-first probe's build (p2) over a HashAggregate (p1) that reads a CTE's store (p0, a VALUES that
    /// may raise) takes the CTE whole before PG runs the outer side (p3 reads the store too), so the CTE is not that side's:
    /// `rest` Safe. A LIMIT in the HashAggregate's producer (it may stop early), or no CTE: `rest` Raises.
    #[test]
    fn a_cte_the_build_reads_whole_through_a_breaker_is_not_the_outer_sides() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::{Bound, InitPlan};
        use gravity_decompose::ops::shared::subplan::SubLink;
        use types_nodes::list::IntList;
        for (cte, limit, want) in [(true, false, Side::Safe), (false, false, Side::Raises), (true, true, Side::Raises)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(PipelineNode::new(a, 1, Source::Values { scanrelid: 1, lists: nil }, into(BreakerKind::Materialize, 0)));
            let mut x = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, into(BreakerKind::HashAgg, 3).consuming_one(0));
            if limit {
                x.push(Op::Limit { count: Bound::Const(5), offset: Bound::All, ties: None });
            }
            g.pipelines.push(x);
            let mut b = PipelineNode::new(a, 3, Source::Breaker { slot: 3 }, into(BreakerKind::HashBuild, 1).consuming_one(3));
            b.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX }));
            g.pipelines.push(b);
            g.pipelines.push(PipelineNode::new(a, 4, Source::Breaker { slot: 0 }, into(BreakerKind::Materialize, 2).consuming_one(0)));
            let mut q = PipelineNode::new(a, 5, Source::Breaker { slot: 2 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(2));
            q.push(Op::Join(JoinDesc::Probe { slot: 1, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 3, outer_first: false, catalog: false }));
            g.pipelines.push(q);
            if cte {
                let params: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
                g.init_plans.push(InitPlan { plan_id: 1, pipeline: 0, link: SubLink::Cte, param: -1, params, slot: Some(0), volatility: Volatility::Immutable, raise_free: false, parallel_safe: true, attach: -1 });
            }
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let o = outer(&gx, 2, 4, 0);
            assert_eq!((o.rest, o.all, o.skip), (want, Side::Raises, 1 << 3), "cte {cte}, limit {limit}");
        }
    }

    /// gen hj-0250: an outer-first self join of a CTE (p0, a VALUES that may raise) whose build (p2) filters by an InitPlan's
    /// parameter: the InitPlan's plan (p1) reads only the CTE's store, quiet, so the parameter reads quiet and `shared_cte`
    /// holds. A plan of its own that may raise (a VALUES source), or a parameter no InitPlan sets, keeps it off.
    #[test]
    fn a_quiet_initplans_parameter_on_a_cte_self_joins_build_is_quiet() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::InitPlan;
        use gravity_decompose::ops::shared::subplan::SubLink;
        use types_nodes::list::IntList;
        use types_nodes::primnodes::{Param, ParamKind};
        let ctx = mcx::MemoryContext::new("hj-0250");
        let mcx = ctx.mcx();
        for (loud, set, want) in [(false, 7, true), (true, 7, false), (false, 8, false)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(PipelineNode::new(a, 1, Source::Values { scanrelid: 1, lists: nil }, into(BreakerKind::Materialize, 0)));
            let plan = if loud { PipelineNode::new(a, 2, Source::Values { scanrelid: 2, lists: nil }, into(BreakerKind::Materialize, 2)) } else { PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, into(BreakerKind::Materialize, 2).consuming_one(0)) };
            g.pipelines.push(plan);
            let param = Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid: 7, paramtype: types_core::BOOLOID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
            let mut b = PipelineNode::new(a, 3, Source::Breaker { slot: 0 }, into(BreakerKind::HashBuild, 1).consuming_one(0));
            b.push(Op::Filter(param));
            b.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX }));
            g.pipelines.push(b);
            let mut q = PipelineNode::new(a, 4, Source::Breaker { slot: 0 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0));
            q.push(Op::Join(JoinDesc::Probe { slot: 1, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 3, outer_first: true, catalog: false }));
            g.pipelines.push(q);
            let none: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
            let params: &IntList<'_> = Box::leak(Box::new(IntList::make1(mcx, set).unwrap()));
            g.init_plans.push(InitPlan { plan_id: 1, pipeline: 0, link: SubLink::Cte, param: -1, params: none, slot: Some(0), volatility: Volatility::Immutable, raise_free: false, parallel_safe: true, attach: -1 });
            g.init_plans.push(InitPlan { plan_id: 2, pipeline: 1, link: SubLink::Expr, param: set, params, slot: Some(2), volatility: Volatility::Immutable, raise_free: false, parallel_safe: true, attach: -1 });
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(own_side(&gx, 2), Side::Raises, "a PARAM_EXEC alone reads as a raise");
            assert_eq!(shared_cte(&gx, 2, 3, 0), want, "loud {loud}, set {set}");
        }
    }

    /// W3-D1486-NONFAM (design review r1 F1): a settled union's emit is fed by EVERY arm, not only its nearest producer
    /// (`life_of`: the last feed). As an outer-first probe's build (the emit ends in the HashBuild), a raising FIRST arm
    /// makes the side Raises though the last arm is a quiet scan (and the arms swapped too; two scans stay Safe). As a
    /// build-first probe's outer (the emit probes), an empty build lets the runner skip every arm after the build.
    #[test]
    fn a_settled_unions_side_and_outer_skip_read_every_arm() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::ops::shared::append::AppendDesc;
        let arena = Arena::new(1 << 17); // D3588: Pipeline 416 -> 424 bytes (source_of) passed 64 KiB
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let arm = |quiet: bool| {
            let mut p = PipelineNode::new(a, 2, if quiet { scan() } else { Source::Values { scanrelid: 2, lists: nil } }, into(BreakerKind::Materialize, 2));
            p.push(Op::Append(AppendDesc::Feed { slot: 2, ordered: false }));
            p
        };
        let emit = |d: Pipeline| {
            let mut p = PipelineNode::new(a, 3, Source::Breaker { slot: 2 }, d.consuming_one(2));
            p.push(Op::Append(AppendDesc::Union { slot: 2, children: 2, prune: None }));
            p
        };
        for (first, last, want) in [(false, true, Side::Raises), (true, false, Side::Raises), (true, true, Side::Safe)] {
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(arm(first));
            g.pipelines.push(arm(last));
            g.pipelines.push(emit(into(BreakerKind::HashBuild, 0)));
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(gx.life_of(2, 2).map(|l| l.0), Some(1), "the emit's nearest producer is the last arm");
            assert_eq!(side(&gx, 2), want, "arms quiet {first} / {last}");
        }
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        g.pipelines.push(arm(true));
        g.pipelines.push(arm(true));
        let mut q = emit(Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        q.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(q);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let o = outer(&gx, 0, 3, 1);
        assert_eq!((o.skip, o.rest, o.all), (1 << 1 | 1 << 2, Side::Safe, Side::Safe), "both arms skipped over an empty build");
    }

    /// D4195 (wfrefuse CE2): a build-first probe after a RIGHT drain (an upper Hash Right Join over a lower one). Over an
    /// empty upper build PostgreSQL never runs the lower join: its build (the drain's source) and the pipeline probing it,
    /// which fills the spool the Drain reads, are both outer side, so the runner skips both, the probe pipeline's raising
    /// source with it. Without the Drain's spool the probe pipeline ran ahead of the empty build.
    #[test]
    fn a_drain_before_the_probe_puts_its_spools_filler_on_the_outer_side() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let probe = |slot| Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_RIGHT, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false });
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        g.pipelines.push(PipelineNode::new(a, 2, scan(), into(BreakerKind::HashBuild, 1)));
        let mut fill = PipelineNode::new(a, 3, Source::Values { scanrelid: 3, lists: nil }, into(BreakerKind::Materialize, 2));
        fill.push(probe(1));
        g.pipelines.push(fill);
        let mut q = PipelineNode::new(a, 4, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(2));
        q.push(Op::Join(JoinDesc::Drain { matches: 2, jointype: JoinType::JOIN_RIGHT }));
        q.push(probe(0));
        g.pipelines.push(q);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let o = outer(&gx, 0, 3, 1);
        assert_eq!((o.skip, o.rest, o.all), (1 << 1 | 1 << 2, Side::Safe, Side::Raises), "the lower build and its probe pipeline skipped over an empty upper build");
        assert!(o.raising.is_empty(), "nothing of the outer side runs ahead: {:?}", o.raising);
    }

    /// M7 L0-b2: a probe pipeline whose own source may raise (a VALUES) runs only after its build, so an empty build lets
    /// the runner skip that pipeline (its bit in `skip`; nothing left in `rest`), for INNER and SEMI; a fill type's
    /// outer runs whatever the build holds (no skip, nothing refused).
    #[test]
    fn a_raising_probe_source_is_skipped_over_an_empty_build() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for jt in [JoinType::JOIN_INNER, JoinType::JOIN_SEMI, JoinType::JOIN_LEFT, JoinType::JOIN_ANTI] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let scan = Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
            let build = PipelineNode::new(a, 1, scan, Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 4096).in_slot(0)), Reason::NoDrivingSite));
            let mut p = PipelineNode::new(a, 3, Source::Values { scanrelid: 2, lists: nil }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(build);
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let o = outer(&gx, 0, 1, 0);
            let fills = matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI);
            assert_eq!((o.skip, o.rest), (if fills { 0 } else { 1 << 1 }, Side::Safe), "{jt:?}");
        }
    }

    /// D3374 (`held_before`): an unfed build op 14 holds for an earlier outer-first probe of the reader (a VALUES build,
    /// the tests' stand-in that may raise) raises nowhere before the reader's later probe, which every row reaches only
    /// past the earlier one; not when its probe comes after the later probe, nor when that probe builds first (no hold).
    #[test]
    fn a_held_build_of_an_earlier_probe_is_held_before_a_later_one() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for (first, outer_first, want) in [(true, true, true), (false, true, false), (true, false, false)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
            let scan = |r: u16| Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: r, relid: 16384 };
            let into = |slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let probe = |slot, of| Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: of, catalog: false });
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(PipelineNode::new(a, 1, Source::Values { scanrelid: 2, lists: nil }, into(0)));
            g.pipelines.push(PipelineNode::new(a, 2, scan(1), into(1)));
            let mut p = PipelineNode::new(a, 3, scan(3), Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            let (lower, upper) = (probe(0, outer_first), probe(1, true));
            if first {
                p.push(lower);
                p.push(upper);
            } else {
                p.push(upper);
                p.push(lower);
            }
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(held_before(&gx, 0, 2, if first { 1 } else { 0 }), want, "earlier {first}, outer-first {outer_first}");
        }
    }

    /// D3374 (rerun_d2194 B4): a pipeline between a lazy group and its reader whose only op is a Limit reads as quiet
    /// there when its bounds are settled (none, or constants of at least 0), and as a raise when one is negative;
    /// `own_side` keeps reading any Limit as a raise.
    #[test]
    fn a_settled_limit_between_a_lazy_group_and_its_reader_is_quiet() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::graph::Bound;
        for (count, want) in [(Bound::Const(2), true), (Bound::All, true), (Bound::Const(-1), false)] {
            let arena = Arena::new(1 << 16);
            let a = arena.handle();
            let mut g = PipelineGraph::new(a);
            let into = |slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::Materialize, 4096).in_slot(slot)), Reason::NoDrivingSite);
            let mut p = PipelineNode::new(a, 1, Source::Breaker { slot: 0 }, into(1).consuming_one(0));
            p.push(Op::Limit { count, offset: Bound::All, ties: None });
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(own_side(&gx, 0), Side::Raises, "own_side, count {count:?}");
            assert_eq!(between_safe(&gx, 0), want, "between, count {count:?}");
        }
    }

    /// W4b-RowX (code review round 3): table_to_xml, table_to_xml_and_xmlschema, schema_to_xml, schema_to_xml_and_xmlschema,
    /// database_to_xml, database_to_xml_and_xmlschema (xml), then the controls table_to_xmlschema (xml) and
    /// pg_relation_is_publishable (bool): (pg_proc oid, prorettype).
    const XML_ETC: [(u32, u32); 8] = [(2923, 142), (2929, 142), (2933, 142), (2935, 142), (2936, 142), (2938, 142), (2926, 142), (6121, 16)];

    /// `rowx`'s catalog: PgCatalog's answers, except that a unit test has no syscache, so a user type a fixture names (an
    /// OID from FIRST_NORMAL_OID on) reads as pg_type has a plain base type: its own base, neither a range nor a multirange
    /// (the side class's shared call test, `side_calls`, asks that of every argument's type: effects.rs `range_user_cmp`).
    struct Fixtures;

    impl<'mcx> gravity_expr::Catalog<'mcx> for Fixtures {
        fn func(&self, f: Oid) -> PgResult<FuncFacts> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::func(&PgCatalog, f)
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::opcode(&PgCatalog, o)
        }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::op_input_types(&PgCatalog, o)
        }
        fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::btree_cmp_proc(&PgCatalog, f, l, r)
        }
        fn ordering_cmp_proc(&self, o: Oid) -> PgResult<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::ordering_cmp_proc(&PgCatalog, o)
        }
        fn type_eq_opr(&self, t: Oid) -> PgResult<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::type_eq_opr(&PgCatalog, t)
        }
        fn agg_finalfn(&self, f: Oid) -> Option<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::agg_finalfn(&PgCatalog, f)
        }
        fn opclass(&self, c: Oid) -> PgResult<(Oid, Oid)> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::opclass(&PgCatalog, c)
        }
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::type_facts(&PgCatalog, t)
        }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::type_cmp_proc(&PgCatalog, t)
        }
        fn base_type(&self, t: Oid, typmod: &mut i32) -> PgResult<Oid> {
            if t >= FIRST_NORMAL_OID { Ok(t) } else { <PgCatalog as gravity_expr::Catalog<'mcx>>::base_type(&PgCatalog, t, typmod) }
        }
        fn domain(&self, t: Oid) -> PgResult<DomainFacts<'mcx>> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::domain(&PgCatalog, t)
        }
        fn io(&self, t: Oid) -> PgResult<IoFacts> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::io(&PgCatalog, t)
        }
        fn text(&self, c: Oid) -> PgResult<TextFacts> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::text(&PgCatalog, c)
        }
        fn tier(&self, f: Oid) -> Tier {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::tier(&PgCatalog, f)
        }
        fn shim(&self, f: Oid) -> Option<&'static Shim> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::shim(&PgCatalog, f)
        }
        fn enum_members(&self, t: Oid) -> Option<Vec<(Oid, f32)>> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::enum_members(&PgCatalog, t)
        }
        fn regex_compiles(&self, pattern: &[u8], icase: bool, c: Oid) -> bool {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::regex_compiles(&PgCatalog, pattern, icase, c)
        }
        fn hash_key(&self, o: Oid, c: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::hash_key(&PgCatalog, o, c)
        }
        fn hash_key_typed(&self, o: Oid, c: Oid, l: Oid, r: Oid) -> PgResult<Result<gravity_fn::typesem::KeySem, gravity_fn::typesem::TypeClass>> {
            <PgCatalog as gravity_expr::Catalog<'mcx>>::hash_key_typed(&PgCatalog, o, c, l, r)
        }
        fn support(&self, q: Support) -> PgResult<Oid> {
            match q {
                Support::RangeSub(t) | Support::MultirangeRange(t) if t >= FIRST_NORMAL_OID => Ok(0),
                q => <PgCatalog as gravity_expr::Catalog<'mcx>>::support(&PgCatalog, q),
            }
        }
    }

    /// W4b-RowX: pipeline 0 over a Const source (`src` 0), an initdb btree Index source keyed IS NOT NULL (1) or by `= ANY`
    /// (2) or a gist one keyed IS NOT NULL (3), or a Const source beside a probe whose hash clause calls a user operator (4),
    /// or a user btree index keyed IS NOT NULL (5), or a Const source beside a pipeline over a user table's index (6) or one
    /// filtering a user-typed (7) or an int4 (8) value IS NOT NULL, or gated by that int4 test in a one-time filter's list
    /// (9), or filtering a builtin call IS NOT NULL (10 + i: `XML_ETC[i]`; 17: in a one-time filter's list), or a Const
    /// source filtering array_upper of a user-typed (18) or an int4[] (19) array IS NOT NULL itself, with a FROM-clause set
    /// op calling `funcid` on `args` (0: none): (`side_by`, `own_side`).
    fn rowx(src: u8, funcid: u32, args: &[types_nodes::Node<'static>]) -> (Side, Side) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::graph::KeyForm;
        use gravity_decompose::ops::srf::kind_of;
        use gravity_decompose::ops::shared::srf::{SrfDesc, SrfFn, SrfPlace};
        use types_nodes::list::OidList;
        use types_nodes::primnodes::{FuncExpr, NullTest, NullTestType, OpExpr};
        use types_nodes::Node;
        let m = Box::leak(Box::new(mcx::MemoryContext::new("rowx"))).mcx();
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let source = match src {
            0 | 4 | 6..=19 => Source::Const,
            _ => {
                let v = Node::mk_var(m, 1, 1, 23, -1, 0, 0).unwrap();
                let value = Node::mk(m, NullTest { arg: Some(v), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).unwrap();
                let mut keys = ArenaVec::new_in(a);
                keys.push(gravity_decompose::IndexKey { attno: 1, col: 1, opno: 0, value, form: if src == 2 { KeyForm::Any } else { KeyForm::NotNull } });
                Source::Index { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 1249, indexid: if src == 5 { 16390 } else { 2659 }, keys, only: false, backward: false, recheck: nil, iam: if src == 3 { gravity_decompose::catalog::IndexAm::Gist } else { gravity_decompose::catalog::IndexAm::Btree } }
            }
        };
        let mut p = PipelineNode::new(a, 1, source, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        if funcid != 0 {
            let mut l = NodeList::nil();
            args.iter().for_each(|&n| l.lappend(m, n).unwrap());
            let node = Node::mk(m, FuncExpr { funcid, funcresulttype: 23, funcretset: true, args: l, ..Default::default() }).unwrap();
            let coltypes: &OidList<'_> = Box::leak(Box::new(OidList::nil()));
            let mut functions = ArenaVec::new_in(a);
            functions.push(SrfFn { resno: 1, node, funcid, args: &node.as_func_expr().unwrap().args, kind: kind_of(funcid, true), cols: 1, coltypes });
            p.push(Op::Srf(SrfDesc { place: SrfPlace::From { scanrelid: 2, ordinality: false }, functions, tlist: nil }));
        }
        if src == 18 || src == 19 {
            let arr = Node::mk_var(m, 1, 1, if src == 18 { 16401 } else { 1007 }, -1, 0, 0).unwrap();
            let one = Node::mk_const(m, 23, -1, 0, 4, datum::Datum::from_i32(1), false, true).unwrap();
            let up = Node::mk(m, FuncExpr { funcid: 2092, funcresulttype: 23, args: NodeList::make2(m, arr, one).unwrap(), ..Default::default() }).unwrap();
            p.push(Op::Filter(Node::mk(m, NullTest { arg: Some(up), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).unwrap()));
        }
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(p);
        if src == 6 {
            let v = Node::mk_var(m, 1, 1, 23, -1, 0, 0).unwrap();
            let value = Node::mk(m, NullTest { arg: Some(v), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).unwrap();
            let mut keys = ArenaVec::new_in(a);
            keys.push(gravity_decompose::IndexKey { attno: 1, col: 1, opno: 0, value, form: KeyForm::NotNull });
            let source = Source::Index { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 2, relid: 16384, indexid: 16390, keys, only: false, backward: false, recheck: nil, iam: gravity_decompose::catalog::IndexAm::Btree };
            g.pipelines.push(PipelineNode::new(a, 2, source, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)));
        }
        if src == 4 {
            let v = |att| Node::mk_var(m, 1, att, 23, -1, 0, 0).unwrap();
            let op = Node::mk(m, OpExpr { opno: 16401, opfuncid: 16400, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(m, &[v(1), v(2)]).unwrap(), location: -1 }).unwrap();
            let clauses: &NodeList<'_> = Box::leak(Box::new(NodeList::from_slice(m, &[op]).unwrap()));
            let mut q = PipelineNode::new(a, 2, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            q.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: true, catalog: false }));
            g.pipelines.push(q);
        }
        if (7..=17).contains(&src) {
            let v = match src {
                10..=17 => Node::mk(m, FuncExpr { funcid: XML_ETC[src as usize - 10].0, funcresulttype: XML_ETC[src as usize - 10].1, ..Default::default() }).unwrap(),
                _ => Node::mk_var(m, 1, 1, if src == 7 { 16400 } else { 23 }, -1, 0, 0).unwrap(),
            };
            let t = Node::mk(m, NullTest { arg: Some(v), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).unwrap();
            let mut q = PipelineNode::new(a, 2, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            q.push(if src == 9 || src == 17 { Op::Gate(Node::mk_list(m, NodeList::from_slice(m, &[t]).unwrap()).unwrap()) } else { Op::Filter(t) });
            g.pipelines.push(q);
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &Fixtures, over: None, mcx: rig.mcx() };
        (side_by(&gx, 0, usize::MAX, false), own_side(&gx, 0))
    }

    /// W4b-RowX (the D1902-class serve-back): `side_by` (D1902 / D1904 / D1906's reading) reads a Const source, a
    /// generate_series(int4, int4) or (int8, int8) of quiet arguments and an initdb btree index's key IS NOT NULL as PG
    /// 18.6 runs them: raising nothing. `own_side` (op 14, D237, D276, the spill) reads them as before B. Does not apply:
    /// the 3-arg series (step 0 raises), numeric's (NaN raises), a raising argument (int4div by zero), a user set function
    /// (a call), a key under ANY (its sort runs the opclass's order proc), a key of another AM than btree, a user index (its
    /// opclass may be a user one over the builtin operators whose FUNCTION 1, the order proc _bt_first calls, raises: code
    /// review round 1), a statement whose hash clause calls a user operator (D2036: PG runs it per bucket match, v4
    /// compares by the key type), a statement that reads a value of a user type (code review round 2: D2065 / D2068), a
    /// statement that calls a row-reading xml mapping builtin (code review round 3: STABLE, its own query runs a view's
    /// nextval); its controls: table_to_xmlschema (no rows) and the describes' STABLE pg_relation_is_publishable.
    #[test]
    fn side_by_reads_quiet_sources_and_series_quiet_and_own_side_reads_them_as_before() {
        use types_nodes::primnodes::FuncExpr;
        let m = Box::leak(Box::new(mcx::MemoryContext::new("rowx-args"))).mcx();
        let int4 = |v: i32| types_nodes::Node::mk_const(m, 23, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap();
        let (one, ten) = (int4(1), int4(10));
        let div = types_nodes::Node::mk(m, FuncExpr { funcid: 154, funcresulttype: 23, args: NodeList::make2(m, one, int4(0)).unwrap(), ..Default::default() }).unwrap();
        let (safe, raises) = ((Side::Safe, Side::Raises), (Side::Raises, Side::Raises));
        assert_eq!(rowx(0, 0, &[]), safe, "a Result's one row");
        assert_eq!(rowx(0, 1067, &[one, ten]), safe, "generate_series(int4, int4)");
        assert_eq!(rowx(0, 1069, &[one, ten]), safe, "generate_series(int8, int8)");
        assert_eq!(rowx(1, 0, &[]), safe, "an initdb btree index's key IS NOT NULL");
        assert_eq!(rowx(1, 1067, &[one, ten]), safe, "an index scan under a series");
        assert_eq!(rowx(0, 1066, &[one, ten, one]), raises, "the 3-arg series");
        assert_eq!(rowx(0, 3260, &[one, ten]), raises, "numeric's series");
        assert_eq!(rowx(0, 1067, &[one, div]), raises, "a raising argument");
        assert_eq!(rowx(0, 16390, &[one, ten]), (Side::UserFn, Side::UserFn), "a user set function");
        assert_eq!(rowx(2, 0, &[]), raises, "a key under ANY");
        assert_eq!(rowx(3, 0, &[]), raises, "a gist key IS NOT NULL (btree sources only)");
        assert_eq!(rowx(5, 0, &[]), raises, "a user index's key IS NOT NULL (its opclass's order proc may be a user one)");
        assert_eq!(rowx(5, 1067, &[one, ten]), raises, "a user index under a series");
        assert_eq!(rowx(4, 0, &[]), raises, "a hash clause calls a user operator: v4 compares by type (D2036, in-row)");
        assert_eq!(rowx(6, 1067, &[one, ten]), raises, "the statement reads a user table's index (its opclass's FUNCTION 1 runs unseen in a kept build)");
        assert_eq!(rowx(7, 1067, &[one, ten]), safe, "another pipeline reads a value of a user type: per expression (main 10-03 22:21Z), not the statement");
        assert_eq!(rowx(18, 1067, &[one, ten]), raises, "its own array_upper over a user-typed array reads as trunk (a builtin over a user type may run its functions)");
        assert_eq!(rowx(19, 1067, &[one, ten]), safe, "its own array_upper over an int4[] reads PG's quiet array read");
        assert_eq!(rowx(8, 1067, &[one, ten]), safe, "the same filter over an int4 value");
        assert_eq!(rowx(9, 1067, &[one, ten]), safe, "a one-time filter's implicit AND list over an int4 value (member by member)");
        for (i, (f, _)) in XML_ETC[..6].iter().enumerate() {
            assert_eq!(rowx(10 + i as u8, 1067, &[one, ten]), safe, "another pipeline calls the row-reading xml mapping builtin {f}: per expression");
        }
        assert_eq!(rowx(16, 1067, &[one, ten]), safe, "table_to_xmlschema (it reads no rows)");
        assert_eq!(rowx(17, 1067, &[one, ten]), safe, "pg_relation_is_publishable in a one-time filter's list (the describes' gate)");
    }

    /// with#170 / #171 (hashjoin owner review 2026-10-04T22:54:16Z): `own_side` reads a VALUES list of quiet expressions
    /// (a CTE's constant rows) and a window `effects::window_quiet` passes as Safe. Does not apply: a VALUES row calling a
    /// user function stays a user function's side and one that may raise (int4div by zero) raises; a window
    /// `window_quiet` refuses (lag, a user partition =) raises as before. (A VOLATILE builtin's volatility comes from the
    /// syscache, which a unit test has not.)
    #[test]
    fn a_quiet_values_list_and_a_quiet_window_read_safe() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::ops::window::{WinFn, WinFnKind, WindowDesc};
        use gravity_decompose::ops::shared::window::{Exclude, FrameBound, FrameCursor, FrameMode};
        use types_nodes::plannodes::WindowAgg;
        use types_nodes::primnodes::{FuncExpr, WindowFunc};
        let m = Box::leak(Box::new(mcx::MemoryContext::new("values-window"))).mcx(); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let a = arena.handle();
        let side = |p: PipelineNode<'static>| {
            let mut g = PipelineGraph::new(a);
            g.pipelines.push(p);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            own_side(&gx, 0)
        };
        let int4 = |v: i32| Node::mk_const(m, 23, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap();
        let call = |funcid: u32, args: &[Node<'static>]| Node::mk(m, FuncExpr { funcid, funcresulttype: 23, args: if args.is_empty() { NodeList::nil() } else { NodeList::from_slice(m, args).unwrap() }, ..Default::default() }).unwrap();
        let values = |x: Node<'static>| {
            let row = Node::mk_list(m, NodeList::from_slice(m, &[int4(1), x]).unwrap()).unwrap();
            let lists: &'static NodeList<'static> = Box::leak(Box::new(NodeList::from_slice(m, &[row]).unwrap())); // alloc: Cold
            side(PipelineNode::new(a, 1, Source::Values { scanrelid: 1, lists }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)))
        };
        assert_eq!(values(int4(2)), Side::Safe, "VALUES (1, 2)");
        assert_eq!(values(call(16390, &[])), Side::UserFn, "VALUES (1, f()): a user function");
        assert_eq!(values(call(154, &[int4(1), int4(0)])), Side::Raises, "VALUES (1, 1 / 0)");
        let cursor = FrameCursor { mode: FrameMode::Rows, start: FrameBound::UnboundedPreceding, end: FrameBound::CurrentRow, exclude: Exclude::None, needs: 0 };
        // (the window function, its kind, partition =)
        let window = |winfnoid: u32, kind: WinFnKind, part: u32, volatile_args: bool| {
            let f = Node::mk(m, WindowFunc { winfnoid, wintype: 20, ..Default::default() }).unwrap();
            let plan: &'static WindowAgg<'static> = Box::leak(Box::new(WindowAgg { partNumCols: 1, partColIdx: &[1], partOperators: Box::leak(Box::new([part])), partCollations: &[0], ordNumCols: 1, ordColIdx: &[1], ordOperators: Box::leak(Box::new([97])), ordCollations: &[0], ..Default::default() })); // alloc: Cold
            let mut functions = ArenaVec::new_in(a);
            functions.push(WinFn { resno: 2, node: f, func: f.as_window_func().expect("a WindowFunc"), kind, cursor });
            let run_condition: &'static NodeList<'static> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
            let scan = Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
            let mut p = PipelineNode::new(a, 1, scan, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
            p.push(Op::Window(WindowDesc { winref: 1, partition: ArenaVec::new_in(a), order: ArenaVec::new_in(a), frame: cursor, functions, natts: 1, run_condition, top: true, tlist: &plan.plan.targetlist, plan, volatile_args, volatile_proj: false, whole: false, arms_capped: false, capped: None }));
            side(p)
        };
        assert_eq!(window(3100, WinFnKind::RowNumber, 96, false), Side::Safe, "row_number() OVER (PARTITION BY int4 ORDER BY int4)");
        assert_eq!(window(3106, WinFnKind::Lag, 96, false), Side::Raises, "lag (its offset may raise): as before");
        assert_eq!(window(3100, WinFnKind::RowNumber, 16602, false), Side::UserFn, "D4145: a user partition = runs user code (window_calls)");
        assert_eq!(window(3106, WinFnKind::Lag, 96, true), Side::UserFn, "D4145: lag(nextval(..)): a VOLATILE argument calls");
    }
}
