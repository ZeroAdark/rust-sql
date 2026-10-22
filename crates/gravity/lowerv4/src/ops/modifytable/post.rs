//! D2772 POST (modifytable, regress triggers#500: `WITH upd AS (UPDATE parted2_stmt_trig SET a = a) UPDATE
//! parted_stmt_trig SET a = a`): PostgreSQL runs a data-modifying CTE nothing reads once the statement's own
//! ModifyTable has ended (ExecPostprocessPlan over `es_auxmodifytables`). The statement's BEFORE STATEMENT and row
//! triggers fire and its AFTER STATEMENT events queue; then the CTE's BEFORE STATEMENT triggers fire, its rows run and its
//! events queue after the statement's. D3449: two such CTEs run apart, the later CTE plan first, each to its end (its
//! AFTER STATEMENT events queued before the next one's BEFORE STATEMENT triggers fire). `set` names the pipelines that
//! run so (each CTE's write and, to a fixpoint, every producer only its group reads: the graph's first ones); `order`
//! moves them to the program's end, group by group in the order they run, and `mark` flags them `Pipeline::post`. Every
//! runner then runs the statement's own pipelines first, in program order, and turns before each group's first pipeline
//! (exec after_query.rs `turn` queues the AFTER STATEMENT events so far, then the group's writer binds: driver
//! entry/post.rs; `start` is where, `WriteAt::end`). A recheck of the own write reruns only pipelines before it, so none
//! of them (`reads_product`), as EvalPlanQual never runs an unread CTE's plan; so the own UPDATE / DELETE beside such a
//! CTE serves (rowop.rs `two_plans_served`) and keeps PostgreSQL's trigger order (rowop.rs APART_ORDER). The own write
//! may return rows (D3449): its emit runs before the post pipelines, as PostgreSQL's portal runs ExecutorFinish once the
//! statement's rows are all made.
//!
//! Fail closed (`set` is `None`, and the statement keeps D1687's order and refusals): none of the statement's own write
//! pipelines is a CTE's (or it has none and a row lock outside every CTE, none inside one, the CTEs' targets firing no
//! trigger: D3379 LOCKPOST); nothing reads a CTE's product, none has a RETURNING list (its rows would go to a store:
//! D2815), no two CTEs write one relation, and a relation the own write and a CTE both write fires no trigger (its facts
//! known: D3379 POSTSAME); no InitPlan but CTE links; no rerun set holds a post pipeline; a post pipeline reads only its
//! group's products, and no slot number is made both inside and outside the set; the set is the graph's first pipelines
//! and not all of them. D3507 (graphile-worker's get_job: `WITH j AS (SELECT .. LIMIT 1 FOR UPDATE SKIP LOCKED), q AS
//! (UPDATE job_queues .. FROM j ..) UPDATE jobs .. FROM j ..`): or a post pipeline also reads the store of a CTE that
//! writes nothing, outside every group (the claim the own write reads too, run before the post groups as the own write
//! pulls it), and the set is then any of the graph's pipelines (`order` moves it last).
//!
//! D3379 POSTSAME (isowrite_drained_cte_order): the CTE may write the own write's relation, the same rows. PostgreSQL's
//! order then shows in the rows: the CTE's scan reads the statement's snapshot, so it meets the own command's versions
//! (an UPDATE / DELETE skips a row the own write changed, TM_SelfModified; an upsert raises; an INSERT of a key the own
//! write moved away meets no conflict), where D1687's order would run it first. A relation both write fires no trigger:
//! for two ModifyTables of one relation and command PostgreSQL fires its BEFORE STATEMENT triggers once, queues one AFTER
//! STATEMENT event (trigger.c `before_stmt_triggers_fired`, `cancel_prior_stmt_triggers`) and shares one transition
//! table, which two writers would not.
//!
//! D3379 CTE-MERGE (mergecmd_cte_merge, regress triggers#863: `WITH w AS (INSERT INTO t VALUES (47)) MERGE INTO t USING
//! (VALUES (46), (47), (48)) .. WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN INSERT ..`, t with an AFTER
//! STATEMENT trigger over a transition table): a relation both write may fire triggers when both writes only insert
//! into it (a plain INSERT or an insert-only MERGE, no ON CONFLICT, no route, its trigger facts known, no INSTEAD OF).
//! The two ModifyTables then meet one (relation, INSERT) pair, which trigger.c keys: one BEFORE STATEMENT firing, one
//! transition table (MakeTransitionCaptureState's CMD_INSERT table, a MERGE's insert one too) and one AFTER STATEMENT
//! event, the later queued one cancelling the earlier. The host's writers call the ports of the same functions (hosts
//! trigger_stmt.rs `bind`, `fire_as`), keyed the same way at the query level: the CTE's writer binds at the turn, after
//! the own write's AFTER STATEMENT event queued (queued, not fired: no table closed, so its capture takes the open
//! table), and finds the BEFORE STATEMENT triggers fired. Its rows come after the own write's in the table, as
//! PostgreSQL's ExecPostprocessPlan adds them.
//!
//! D3379 LOCKPOST (isowrite_drained_cte_order): a SELECT .. FOR UPDATE beside such a CTE. PostgreSQL runs the CTE once
//! the statement's LockRows has ended, so the lock meets the old rows and the CTE the locked ones (its own transaction's
//! locks: no wait); in D1687's order the CTE's write would come first and the lock would skip the rows it changed
//! (TM_SelfModified). The lock is the program's write plan, the CTE's write one beside it bound at the turn (rowop.rs
//! `lock_beside_drained`), and the lock's recheck reruns none of the post pipelines (`reads_product`).

use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::{Op, PipelineNode};
use gravity_program::{Cmd, Pipeline};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::graph::Gx;

/// The post pipelines of the statement's graph, in graph order (module doc); `None` when it has none or they cannot run
/// so.
pub(crate) fn set<'g>(gx: &Gx<'g, '_>) -> Option<PgVec<'g, usize>> {
    groups(gx).map(|g| {
        let mut v: PgVec<'g, usize> = g.into_iter().flatten().collect_in(gx.mcx);
        v.sort_unstable();
        v
    })
}

/// The post groups (module doc), in the order they run: each unread data-modifying CTE's write pipeline first, then the
/// producers only its group reads, to a fixpoint. PostgreSQL's ExecPostprocessPlan runs `es_auxmodifytables` in list
/// order, which ExecInitModifyTable builds with `lcons` as the CTE plans initialize in subplan order: the later CTE plan
/// (the higher plan id) runs first, each to its end. Their union is the graph's first pipelines `0..m`.
fn groups<'g>(gx: &Gx<'g, '_>) -> Option<PgVec<'g, PgVec<'g, usize>>> {
    let mcx = gx.mcx;
    let ps = gx.pipelines();
    let n = ps.len();
    let g = gx.graph;
    let write = |k: usize| ps[k].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }).filter(|w| w.cmd != Cmd::Select);
    // every InitPlan a CTE's link (`cte_write` names a CTE link's pipeline); the program serial: `mark`'s
    if !g.init_plans.iter().all(|i| gx.cte_write(i.pipeline as usize)) {
        return None;
    }
    let ctes: PgVec<'_, usize> = (0..n).filter(|&k| gx.cte_write(k) && write(k).is_some()).collect_in(mcx);
    if ctes.is_empty() {
        return None;
    }
    let readers = |q: usize| ps[q].desc.produces().map_or_else(|| PgVec::<usize>::new_in(mcx), |s| (0..n).filter(|&m| m != q && ps[m].desc.consumes.contains(&s)).collect_in(mcx));
    let rels = |w: &WriteDesc<'_>| w.targets.iter().map(|t| t.relid).chain(core::iter::once(w.plan.target_rel)).collect_in(mcx);
    // each consumed slot's producer: its declared life, else the nearest earlier producer of the slot
    let producer = |q: usize, j: usize, s: u8| match ps[q].desc.consumes_from.get(j) {
        Some(l) => Some(l.0 as usize),
        None => (0..q).rev().find(|&x| ps[x].desc.produces() == Some(s)),
    };
    // D3507: beside a claim inside a CTE that writes nothing (its lock there), a producer's readers are those of its
    // life (`producer`), a slot number another life reuses (the own write's hash build beside q's, the claim's sort
    // read past q's own unread store) no read of it
    let claimish = (0..n).any(|x| gx.cte_write(x) && write(x).is_none() && ps[x].ops.iter().any(|o| matches!(o, Op::Lock(_))));
    let life_readers = |q: usize| (0..n).filter(|&r| r != q && ps[r].desc.consumes.iter().enumerate().any(|(j, &s)| producer(r, j, s) == Some(q))).collect_in(mcx);
    // each CTE unread, no Append arm, no RETURNING list, its targets none of another CTE's (two ModifyTables of one
    // relation would share its statement triggers and the rows one wrote: fail closed)
    let mut crels: PgVec<'_, u32> = PgVec::new_in(mcx);
    for &c in &ctes {
        let cw = write(c)?;
        if ps[c].ops.iter().any(crate::graph::is_arm) || !if claimish { life_readers(c) } else { readers(c) }.is_empty() {
            return None;
        }
        // D2815: a CTE with a RETURNING list stores its rows (nest.cte's pulled set, drained at the statement's end:
        // ops/ctescan/lower.rs DRAIN); the drain pulls the store's pipeline, which this order would make the program's
        // last, an emit to the driver (pull.rs `is_emit`), so its pull refuses as a Bug where PostgreSQL answers
        if cw.returning.is_some() {
            return None;
        }
        let r = rels(cw);
        if r.iter().any(|x| crels.contains(x)) {
            return None;
        }
        crels.extend_in(r);
    }
    // the statement's own write: every other write pipeline is one of its ModifyTable's
    let own: PgVec<'_, &WriteDesc<'_>> = (0..n).filter(|k| !ctes.contains(k)).filter_map(write).collect_in(mcx);
    // D3379 POSTSAME: a relation both write fires no trigger (module doc); D3379 CTE-MERGE: or both only insert into it
    let quiet = |r: &u32| gx.facts.trig_ext(*r) == TrigExt::NONE;
    let inserts = |w: &WriteDesc<'_>| {
        let only = w.cmd == Cmd::Insert && w.merge.is_none() || w.cmd == Cmd::Merge && w.merge.as_ref().is_some_and(|m| super::merge::nm::insert_only(m));
        only && w.on_conflict.is_none() && w.route.is_none() && w.targets.len() <= 1
    };
    let shared = |w: &WriteDesc<'_>, r: &u32| {
        let t = gx.facts.trig_ext(*r);
        let into = |c: &usize| write(*c).is_some_and(|cw| rels(cw).contains(r));
        inserts(w) && t.known && t.instead_row == 0 && ctes.iter().filter(|c| into(c)).all(|&c| write(c).is_some_and(inserts))
    };
    // D3379 LOCKPOST: or no write of the statement's own but its row lock, outside every CTE (none inside one), beside
    // CTEs whose targets fire no trigger (module doc)
    let lock = |k: usize| ps[k].ops.iter().any(|o| matches!(o, Op::Lock(_)));
    let locks = own.is_empty() && (0..n).any(|k| !gx.cte_write(k) && lock(k)) && !(0..n).any(|k| gx.cte_write(k) && lock(k)) && crels.iter().all(quiet);
    match own.first() {
        None if !locks => return None,
        None => {}
        Some(first) => {
            let same = |w: &WriteDesc<'_>| w.cmd == first.cmd && w.targets.iter().map(|t| t.relid).eq(first.targets.iter().map(|t| t.relid));
            if !own.iter().all(|w| same(w) && rels(w).iter().filter(|r| crels.contains(r)).all(|r| quiet(r) || shared(w, r))) {
                return None;
            }
        }
    }
    // each CTE's write and every producer only its group reads, to a fixpoint (one read by two groups joins none)
    let mut groups: PgVec<'_, PgVec<'_, usize>> = ctes.iter().map(|&c| mcx::pg_vec![in mcx; c]).collect_in(mcx);
    loop {
        let fed = |q: usize, gr: &[usize]| {
            let r = if claimish { life_readers(q) } else { readers(q) };
            !r.is_empty() && r.iter().all(|x| gr.contains(x))
        };
        let more: PgVec<'_, (usize, usize)> = (0..n).filter(|q| !groups.iter().any(|gr| gr.contains(q))).filter_map(|q| groups.iter().position(|gr| fed(q, gr)).map(|i| (i, q))).collect_in(mcx);
        if more.is_empty() {
            break;
        }
        more.into_iter().for_each(|(i, q)| groups[i].push_in(q));
    }
    let mut post: PgVec<'_, usize> = groups.iter().flatten().copied().collect_in(mcx);
    post.sort_unstable();
    let m = post.len();
    // D3507: a read of a store of a CTE that writes nothing, outside every group (`claim_store`)
    let claim_store = |x: usize| claimish && !post.contains(&x) && gx.cte_write(x) && write(x).is_none() && !ps[x].ops.iter().any(|o| matches!(o, Op::Write(_)));
    let prefix = post.iter().enumerate().all(|(i, &q)| i == q);
    if m >= n || !prefix && !post.iter().any(|&q| ps[q].desc.consumes.iter().enumerate().any(|(j, &s)| producer(q, j, s).is_some_and(claim_store))) {
        return None;
    }
    // a post pipeline reads only its group's products (`producer`); no slot number is made both by a post pipeline and
    // by another (the driver frees by index)
    let reads_own = groups.iter().all(|gr| gr.iter().all(|&q| ps[q].desc.consumes.iter().enumerate().all(|(j, &s)| producer(q, j, s).is_some_and(|x| gr.contains(&x) || claim_store(x)))));
    let made = |q: usize| ps[q].desc.produces();
    // D3507: or a slot number another pipeline makes too whose every reader is outside the set (the own write's hash
    // build beside q's: `order` runs that life to its end before the set's first pipeline makes the number again)
    let readers_of = |x: usize| (0..n).filter(move |&r| ps[r].desc.consumes.iter().enumerate().any(|(j, &s)| Some(s) == made(x) && producer(r, j, s) == Some(x)));
    let ended = |x: usize| !prefix && readers_of(x).all(|r| !post.contains(&r)) && !claim_store(x);
    let apart = post.iter().filter_map(|&q| made(q)).all(|s| (0..n).filter(|x| !post.contains(x)).all(|x| made(x) != Some(s) || ended(x)));
    let reruns = g.reruns.iter().all(|r| r.pipelines.iter().all(|&q| !post.contains(&(q as usize))));
    if !(reads_own && apart && reruns) {
        return None;
    }
    if own.iter().any(|w| rels(w).iter().any(|r| crels.contains(r))) {
        fires::fire("modifytable.post_same"); // shapecheck fire signal (D3379 POSTSAME): the post CTE writes the own write's relation
    }
    if locks {
        fires::fire("modifytable.post_lock"); // shapecheck fire signal (D3379 LOCKPOST): the post CTE beside the statement's row lock
    }
    let plan_id = |c: usize| g.init_plans.iter().find(|i| i.pipeline as usize == c && matches!(i.link, SubLink::Cte)).map_or(0, |i| i.plan_id);
    groups.iter_mut().for_each(|gr| gr[1..].sort_unstable());
    groups.sort_by_key(|gr| core::cmp::Reverse(plan_id(gr[0])));
    Some(groups)
}

/// Where the post group of CTE write pipeline `k` starts in the program (`at`: a graph pipeline's program index, as in
/// `mark`): its writer binds at the execution's turn before that pipeline (`WriteAt::end`; driver entry/post.rs). `None`:
/// `k` writes in no post group.
pub(crate) fn start(gx: &Gx<'_, '_>, k: usize, at: &dyn Fn(usize) -> Option<usize>) -> Option<usize> {
    let groups = groups(gx)?;
    let gr = groups.iter().find(|gr| gr[0] == k)?;
    let mut best: Option<usize> = None;
    for &q in gr.iter() {
        let i = at(q)?;
        best = Some(best.map_or(i, |b| b.min(i)));
    }
    best
}

/// D652 (`Gx::reads_product`) with the post pipelines left out (marks.rs, for a write): pipeline `k`'s write reads another
/// pipeline's product, a post one never (they run after it, so its recheck reruns none of them).
pub(crate) fn reads_product(gx: &Gx<'_, '_>, k: usize) -> bool {
    if !gx.reads_product(k) {
        return false;
    }
    let Some(post) = set(gx).filter(|s| !s.contains(&k)) else { return true };
    let arm = |x: &PipelineNode<'_>| x.ops.iter().any(crate::graph::is_arm);
    gx.pipelines().iter().enumerate().any(|(q, x)| q != k && !post.contains(&q) && (x.desc.produces().is_some() || !arm(x)))
}

/// Write pipeline `k` is the statement's own beside post pipelines (rowop.rs `two_plans_served`: its recheck reruns no
/// writing CTE).
pub(crate) fn apart(gx: &Gx<'_, '_>, k: usize) -> bool {
    set(gx).is_some_and(|s| !s.contains(&k))
}

/// The statement serves only through the post order: an own UPDATE / DELETE beside the CTE (D652 and nest.cte without
/// it), or trigger events the D1687 order would move (rowop.rs APART_ORDER).
fn relied(gx: &Gx<'_, '_>) -> bool {
    let ps = gx.pipelines();
    super::rowop::apart_order_lost(gx) || (0..ps.len()).any(|k| !gx.cte_write(k) && ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if matches!(w.cmd, Cmd::Update | Cmd::Delete))))
}

/// The post pipelines moved to the program's end (frame/program.rs `emit`, before the pipelines' functions are made;
/// `to_program` remaps every make-time pipeline reference): each post graph pipeline, with the passes made right after it
/// (a virtual id past the graph's pipelines), after every other pipeline, both kept in their order. `k` is an item's graph
/// pipeline or virtual id. Nothing moves when `set` is `None`.
pub(crate) fn order<T>(gx: &Gx<'_, '_>, items: &mut Vec<T>, k: impl Fn(&T) -> usize) {
    let Some(groups) = groups(gx) else { return };
    let n = gx.pipelines().len();
    let rank = |q: usize| groups.iter().position(|gr| gr.contains(&q));
    // a pass (a virtual id) moves with the graph pipeline it follows, in its group
    let moved: PgVec<'_, Option<usize>> = items.iter().scan(None, |post, m| Some(if k(m) < n { *post = rank(k(m)); *post } else { *post })).collect_in(gx.mcx);
    if moved.iter().all(Option::is_some) || moved.iter().all(Option::is_none) {
        return;
    }
    let mut head = mcx::reserved_in(items.len(), gx.mcx);
    let mut tail: PgVec<'_, (usize, T)> = PgVec::new_in(gx.mcx);
    for (m, r) in items.drain(..).zip(moved) {
        match r {
            Some(r) => tail.push_in((r, m)),
            None => head.push_in(m),
        }
    }
    // the groups in the order they run, each group's items kept in their order
    tail.sort_by_key(|&(r, _)| r);
    items.extend(head.into_iter().chain(tail.into_iter().map(|(_, m)| m))); // alloc: Program
}

/// The program's post pipelines flagged (frame/program.rs, once the pipelines are assembled in program order): `at(k)`
/// is graph pipeline `k`'s program index. `order` put the set last; when it is not (no program pipeline of a post graph
/// pipeline, or another graph pipeline's after one), a statement that serves only through the post order refuses by name
/// and any other keeps D1687's order (nothing flagged). True when it flagged them.
pub(crate) fn mark(gx: &Gx<'_, '_>, at: &dyn Fn(usize) -> Option<usize>, pipelines: &mut [Pipeline], node: i32) -> bool {
    let Some(s) = set(gx) else { return false };
    let t = s.iter().map(|&k| at(k).unwrap_or(0)).min().unwrap_or(0);
    let ok = t > 0 && s.iter().all(|&k| at(k).is_some()) && (0..gx.pipelines().len()).filter(|j| !s.contains(j)).all(|j| at(j).is_none_or(|i| i < t));
    if !ok {
        if relied(gx) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("a data-modifying CTE nothing reads beside the statement's own write, whose pipelines the program cannot run after the statement's own (D2772)") });
        }
        return false;
    }
    pipelines[t..].iter_mut().for_each(|p| p.post = true);
    true
}
