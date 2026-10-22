//! Construct C11, one lock plan (lanes/wfrefuse/row-lock-forms.md §5): the home of the decisions about a statement's row
//! locks (lint/rules/rf_lock_forms.py names the ones not moved here yet). Its first answer: the out block's rows.
use gravity_decompose::ops::shared::modifytable::{LockDesc, RowMark};
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_program::program::LockSite;
use gravity_program::LockKind;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::plannodes::RowMarkType;

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::lockrows::lock_init::plain_init;
use crate::rerun::epq::inits::init_ctes;

/// D4205: the rows one push of the out block carries. PostgreSQL sends each row (printtup: the client encoding's
/// conversion, then the socket) before ExecLockRows locks the next one, so in a statement that takes row locks every row
/// is pushed as it is made: a later lock that waits or fails comes after the earlier rows' conversion and send. Any other
/// statement keeps the block (`OUT_ROWS`).
pub(crate) fn out_rows(gx: &Gx<'_, '_>) -> u32 {
    let ps = gx.pipelines();
    let locks = |ops: &[Op<'_>]| ops.iter().any(|o| matches!(o, Op::Lock(_)));
    match ps.iter().any(|p| locks(&p.ops)) || ps.inners().iter().any(|x| locks(&x.1.ops)) {
        true => 1,
        false => crate::frame::px::OUT_ROWS,
    }
}

/// D3775 COPYPAIR (plan.rs `Plan::role`; JOBQ's IN form, `write_pair`): whether lock op `j` of graph pipeline `q` runs
/// below the copy entry of a SubqueryScan whose row `marks` carry in a ROW_MARK_COPY mark. A recheck by those marks
/// hands that SubqueryScan its stored row (copy_row.rs) and never runs the subquery's LockRows again (PostgreSQL's
/// EvalPlanQualFetchRowMark copy leg).
pub(crate) fn below_copy(gx: &Gx<'_, '_>, q: usize, j: usize, marks: &[RowMark]) -> bool {
    let copy = |m: &RowMark| m.mark == RowMarkType::ROW_MARK_COPY;
    let below = |m: &RowMark| gx.graph.record_rows.iter().any(|r| r.scanrelid == m.rti && crate::rerun::epq::copy_row::boundary(gx, q, r).is_some_and(|at| j < at));
    q < gx.pipelines().len() && marks.iter().any(|m| copy(m) && below(m))
}

/// The census of what else a statement holds beside its row locks: the InitPlans' pipelines, the pipelines each form
/// counts beside a lock, the CTE claims (lanes/wfrefuse/row-lock-forms.md §5).
pub(crate) struct Census<'a, 'g, 'p>(pub(crate) &'a Gx<'g, 'p>);

impl<'g> Census<'_, 'g, '_> {
    /// Every non-CTE InitPlan's pipeline and, transitively, the producers of the slots they read (nest/initplan.rs `members`).
    pub(crate) fn init_pipelines(&self) -> PgVec<'g, usize> {
        let gx = self.0;
        gx.with_producers(gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte)).map(|i| i.pipeline as usize))
    }

    /// Whether pipeline `x` runs for a non-CTE InitPlan (`init_pipelines`).
    pub(crate) fn init_member(&self, x: usize) -> bool {
        let gx = self.0;
        !gx.graph.init_plans.is_empty() && self.init_pipelines().contains(&x)
    }

    /// CTESORT (`breaker_limit`): every InitPlan of the statement is a CTE's.
    pub(crate) fn ctes_only(&self) -> bool {
        let gx = self.0;
        gx.graph.init_plans.iter().all(|i| matches!(i.link, SubLink::Cte))
    }

    /// AGA-35 (`breaker_limit`, lockrows 15b): the number of pipelines the statement's InitPlans run (`init_pipelines`)
    /// when there are none, or when none is a CTE's, none holds pipeline `k` (the lock's) or `q` (its producer), and the
    /// program's one row lock is the statement's and nothing writes: an InitPlan that reads only. EvalPlanQual copies an
    /// evaluated InitPlan's value and never runs it again (EvalPlanQualStart), so beside such an InitPlan the 15b recheck is
    /// the same; `admit_ir_at` then refuses by name where the recheck could find one pending. Else none.
    /// D3379 INITCTE: or a CTE's that only those InitPlans read and that writes only by INSERT (`init_ctes`): its pipelines
    /// are among theirs (counted in the number) and its INSERTs the only writes.
    pub(crate) fn plain_inits(&self, k: usize, q: usize) -> Option<usize> {
        let gx = self.0;
        if gx.graph.init_plans.is_empty() {
            return Some(0);
        }
        let ctes = init_ctes(gx)?;
        let ps = gx.pipelines();
        let init = self.init_pipelines();
        let ops = || ps.iter().chain(ps.inners().into_iter().map(|x| x.1)).flat_map(|p| p.ops.iter());
        let locks = ops().filter(|o| matches!(o, Op::Lock(_))).count();
        let write = |o: &Op<'_>| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select);
        let writes = ps.iter().enumerate().any(|(x, p)| !ctes.contains(&x) && p.ops.iter().any(write)) || ps.inners().iter().any(|x| x.1.ops.iter().any(write));
        (!init.contains(&k) && !init.contains(&q) && locks == 1 && !writes).then_some(init.len())
    }

    /// D3379 INITSINK (`breaker_limit`, siblings_aga35_15b_initplan's lock bound: `.. WHERE prio >= (SELECT min_prio
    /// FROM cfg WHERE .. FOR UPDATE) ORDER BY .. LIMIT n FOR UPDATE`): `plain_inits` for the 15b lock B of the M4 pair (`mark`),
    /// the statement's two row locks this one and A, A's in a non-CTE InitPlan's pipelines and neither `k` nor `q` among them,
    /// no CTE and no write. PostgreSQL's LockRows of the InitPlan rechecks under its own EvalPlanQual inside the InitPlan's run
    /// and B's EvalPlanQual copies the InitPlan's value (EvalPlanQualStart), never running it or its lock again; here A is
    /// `Program.locks` entry 1 rechecked in the IR (`mark`'s form) and B entry 2 (`Plan::form`, 15b's branch). The number
    /// of pipelines the InitPlans run, else none.
    pub(crate) fn pair_inits(&self, k: usize, q: usize) -> Option<usize> {
        let gx = self.0;
        let ps = gx.pipelines();
        if gx.graph.init_plans.is_empty() || gx.graph.init_plans.iter().any(|i| matches!(i.link, SubLink::Cte)) {
            return None;
        }
        let init = self.init_pipelines();
        let inners = ps.inners();
        let locks: PgVec<'_, usize> = ps.iter().enumerate().flat_map(|(x, p)| p.ops.iter().filter(|o| matches!(o, Op::Lock(_))).map(move |_| x)).collect_in(gx.mcx);
        let inner_locks = inners.iter().any(|x| x.1.ops.iter().any(|o| matches!(o, Op::Lock(_))));
        let writes = ps.iter().chain(inners.iter().map(|x| x.1)).flat_map(|p| p.ops.iter()).any(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select));
        if inner_locks {
            return None;
        }
        let pair = matches!(&locks[..], &[a, b] if a != b && [a, b].contains(&k) && init.contains(if a == k { &b } else { &a }));
        (pair && !init.contains(&k) && !init.contains(&q) && !writes).then_some(init.len())
    }

    /// `plain_inits`, else `pair_inits`: the AGA-35 forms' count of the InitPlans' pipelines beside lock `k` over `q`.
    pub(crate) fn inits(&self, k: usize, q: usize) -> Option<usize> {
        self.plain_inits(k, q).or_else(|| self.pair_inits(k, q))
    }

    /// AGA-35 form 6 (lockrows SUBLOCK, rerun/epq/sub_lock.rs `form`): `plain_inits` for a lock inside SubPlan pipeline `k`'s
    /// inner, whose `form` counts the statement's locks (one) and its writes (only `k`'s own UPDATE / DELETE) itself: the number
    /// of pipelines the InitPlans run when none is a CTE's, none holds `k` and none is attached inside an inner of the
    /// statement, else none (D1018 keeps those). EvalPlanQualStart initializes every SubPlan of the statement in the recheck's
    /// estate, so an InitPlan attached inside the SubPlan's plan (at its LockRows or below) is re-armed there and runs again where
    /// the recheck reads it (a VOLATILE one is called again; witness lockrows-aga35-sublock-initplan). One of the outer query is
    /// attached at its top, outside every SubPlan, and the inner's recheck never reads it. lower.rs `sub_lock` then asks
    /// `admit_sub`.
    pub(crate) fn sub_inits(&self, k: usize) -> Option<usize> {
        let gx = self.0;
        if gx.graph.init_plans.is_empty() {
            return Some(0);
        }
        let inners = gx.pipelines().inners();
        let inner = |i: &gravity_decompose::InitPlan<'_>| inners.iter().any(|x| x.1.node_ops.iter().any(|n| n.node_id == i.attach));
        if gx.graph.init_plans.iter().any(|i| matches!(i.link, SubLink::Cte) || inner(i)) {
            return None;
        }
        let init = self.init_pipelines();
        (!init.contains(&k)).then_some(init.len())
    }

    /// AGA-35 form 7 (lower.rs `subs_in_ir`, D652-S1's lock half): the pipelines the statement's InitPlans run (`init_pipelines`)
    /// beside a lock in pipeline `k` over per-row SubPlan steps whose rerun sets' pipelines `member` names, when every InitPlan
    /// reads only (`plain_inits`: none a CTE's, none holding `k`, the statement's one row lock, no write) and none is attached
    /// inside a SubPlan step's plan or an inner, nor runs in a rerun set; else none. EvalPlanQualStart initializes every
    /// SubPlan in the recheck's estate, so an InitPlan attached inside a step's plan is re-armed there and runs again (a
    /// VOLATILE one is called again): D3272, the IR recheck arms one so (`arms`); one of the outer query is attached at its
    /// top, its value copied, and `admit_ir` asks where it is read. D3655: or beside the InitPlan whose own row lock is the M4
    /// pair's A (`pair_inits`; `mark` gives this lock entry 2): PostgreSQL rechecks A under its own EvalPlanQual inside the
    /// InitPlan's run, and this lock's EvalPlanQual copies the InitPlan's value.
    pub(crate) fn subs_inits(&self, k: usize, member: &dyn Fn(usize) -> bool) -> Option<PgVec<'g, usize>> {
        let gx = self.0;
        if gx.graph.init_plans.is_empty() {
            return Some(PgVec::new_in(gx.mcx));
        }
        self.inits(k, k)?;
        // D3436 INITCTE stays out of this form (fail closed: no witness reads a CTE beside SubPlan steps)
        if !init_ctes(gx)?.is_empty() {
            return None;
        }
        let ps = gx.pipelines();
        let inners = ps.inners();
        let at = |q: usize, i: &gravity_decompose::InitPlan<'_>| ps[q].node_id == i.attach || ps[q].node_ops.iter().any(|n| n.node_id == i.attach);
        let inner = |i: &gravity_decompose::InitPlan<'_>| inners.iter().any(|x| x.1.node_ops.iter().any(|n| n.node_id == i.attach));
        // D3272: one attached inside a step's plan or an inner that the recheck arms afresh (`arms`, decompose `epq_fresh`)
        let fresh = |i: &gravity_decompose::InitPlan<'_>| gx.graph.epq_fresh.contains(&i.plan_id) && plain_init(gx, i);
        if gx.graph.init_plans.iter().any(|i| (inner(i) || (0..ps.len()).any(|q| member(q) && at(q, i))) && !fresh(i)) {
            return None;
        }
        let init = self.init_pipelines();
        (!init.iter().any(|&q| member(q))).then_some(init)
    }

    /// CTELOCK (`write_pair`): pipeline `k`'s lock is inside a CTE that writes nothing, its store read whole by the
    /// statement (each reader outside the CTEs scans it as its source, no nest inner, no LIMIT / OFFSET outside the CTEs;
    /// D3650: a nest inner too under the lock's LIMIT 1); or CTELIMIT's INSERT under a LIMIT of constants
    /// (ops/shared/limit.rs `cte_lock_cap`).
    pub(crate) fn cte_whole(&self, k: usize, limit_op: fn(&Op<'_>) -> bool) -> bool {
        let gx = self.0;
        let ps = gx.pipelines();
        let ctes = gx.with_producers(gx.graph.init_plans.iter().filter(|i| matches!(i.link, SubLink::Cte)).map(|i| i.pipeline as usize));
        let store = ps[k].desc.produces().filter(|_| gx.cte_write(k) && !ps[k].ops.iter().any(|o| matches!(o, Op::Write(_))));
        let scans = |p: &PipelineNode<'_>, s: u8| matches!(p.source, Source::Breaker { slot } if slot == s);
        // D3650 (graphile-worker's get_job with one named queue: q's UPDATE a Nested Loop over job_queues, the CTE Scan of
        // j its inner): or a nest's inner reads the store too, the lock under a LIMIT 1 (its first pull takes every lock
        // the CTE takes: nodeLimit never pulls the LockRows past the first row) and a reader outside the CTEs scanning the
        // store as its source (read whole, as above)
        let first = |d: crate::frame::iface::Demand<'_, '_>| !d.ties && d.rows() == Some(1) && matches!(d.offset, gravity_decompose::graph::Bound::All | gravity_decompose::graph::Bound::Const(0));
        let one = ps[k].ops.iter().skip_while(|o| !matches!(o, Op::Lock(_))).filter_map(crate::frame::iface::demand).any(first);
        let whole = |s: u8| {
            let mut outside = ps.iter().enumerate().filter(|(q, _)| !ctes.contains(q)).map(|(_, p)| p);
            let read = ps.iter().enumerate().any(|(q, p)| !ctes.contains(&q) && scans(p, s));
            outside.all(|p| !p.ops.iter().any(limit_op) && (!p.desc.consumes.contains(&s) || scans(p, s))) && (ps.inners().iter().all(|x| !scans(x.1, s)) || one && read)
        };
        store.is_some_and(whole) || crate::ops::shared::limit::cte_lock_cap(gx, k).is_some()
    }

    /// D3558 (PgQueuer's dequeue: `WITH next_job_queued AS (SELECT id FROM pgq WHERE .. ORDER BY priority DESC, id LIMIT 5 FOR
    /// UPDATE SKIP LOCKED), next_job_retry AS (.. LIMIT 5 FOR UPDATE SKIP LOCKED), .., updated AS (UPDATE pgq .. WHERE id = ANY
    /// (SELECT id FROM combined_jobs) RETURNING *) ..`): the pipelines of the program's LockRows, in order, when there are
    /// several and each is a claim inside a CTE that writes nothing: the CTE's own pipeline, its one locking mark on a plain
    /// relation, a LIMIT / OFFSET above the lock, and its store read whole by scans of it (no reader with a LIMIT / OFFSET,
    /// none a nest's inner). PostgreSQL runs each CTE's LockRows under its own EvalPlanQual as its readers pull it; here each
    /// is a `Program.locks` entry of its own (`Census::claim_entry`), rechecked in the IR. Else none.
    pub(crate) fn cte_claims(&self, limit_op: fn(&Op<'_>) -> bool) -> Option<PgVec<'g, usize>> {
        let gx = self.0;
        let ps = gx.pipelines();
        let inners = ps.inners();
        if inners.iter().any(|x| x.1.ops.iter().any(|o| matches!(o, Op::Lock(_)))) {
            return None;
        }
        let scans = |p: &PipelineNode<'_>, s: u8| matches!(p.source, Source::Breaker { slot } if slot == s);
        let mut out: PgVec<'g, usize> = PgVec::new_in(gx.mcx);
        for (x, p) in ps.iter().enumerate() {
            for (i, o) in p.ops.iter().enumerate() {
                let Op::Lock(d) = o else { continue };
                let s = p.desc.produces()?;
                let plain = d.family().is_none() && d.family_top().is_none() && d.marks.len() == 1 && d.marks.iter().all(|m| m.lock != LockKind::None && !m.parent);
                if !gx.cte_write(x) || !plain || p.ops.iter().any(|o| matches!(o, Op::Write(_))) || !p.ops[i + 1..].iter().any(limit_op) {
                    return None;
                }
                let read = ps.iter().all(|r| !r.desc.consumes.contains(&s) || scans(r, s) && !r.ops.iter().any(limit_op)) && inners.iter().all(|r| !scans(r.1, s));
                if !read {
                    return None;
                }
                out.push_in(x);
            }
        }
        (out.len() > 1).then_some(out)
    }

    /// Whether the statement's pipelines are exactly the `counted` ones a form names (the lock's, its producers and the
    /// sets it counts out): every form's pipeline-count sum asks it.
    pub(crate) fn whole(&self, counted: usize) -> bool {
        self.0.pipelines().len() == counted
    }

    /// INCAGG (D3409, `breaker_limit`, lockrows 15b): the number of pipelines that only read the lock pipeline `k`'s product, at
    /// any depth (an aggregate, a sort or a join's emit over the locked subquery's rows: `SELECT count(*) FROM (SELECT ..
    /// ORDER BY .. LIMIT n FOR UPDATE) s`, PostgreSQL's nodes above Limit -> LockRows), none of them locking or writing and
    /// each reading no slot but that product's chain; 0 when `k` produces nothing. EvalPlanQual runs below LockRows only (the
    /// recheck reruns Sort -> scan over the test tuple), and the rechecked row goes on through the Limit and every node above
    /// it, as the IR recheck's row goes on through pipeline `k`'s steps into its product (D3779 REFNEST-FEED: ref_nest.rs
    /// `form` counts them too). Else none.
    pub(crate) fn above_only(&self, k: usize) -> Option<usize> {
        let gx = self.0;
        let ps = gx.pipelines();
        let Some(first) = ps[k].desc.produces() else { return Some(0) };
        let mut slots = mcx::pg_vec![in gx.mcx; first];
        let mut n = 0;
        for x in k + 1..ps.len() {
            let reads = ps[x].desc.consumes;
            if !reads.iter().any(|s| slots.contains(s)) {
                continue;
            }
            if !reads.iter().all(|s| slots.contains(s)) || ps[x].ops.iter().any(|o| matches!(o, Op::Lock(_) | Op::Write(_))) {
                return None;
            }
            n += 1;
            if let Some(s) = ps[x].desc.produces() {
                slots.push_in(s);
            }
        }
        Some(n)
    }

    /// D3379 FEEDABOVE (`breaker_limit`, lockrows 15b; lockrows_q12's `SELECT .. FROM (SELECT .. ORDER BY prio, id LIMIT 3) s
    /// JOIN lqv w ON w.id = s.wid FOR UPDATE OF s` as a nested loop over the locked subquery, its inner a Materialize of w's
    /// scan): the number of pipelines whose one product only an op of the lock pipeline `k` above its lock op `j` reads (a
    /// nest's inner spool, a build a probe above the lock reads), each reading no slot, locking and writing nothing, and none
    /// an InitPlan's or the Sort's producer `q`. PostgreSQL's EvalPlanQual runs below LockRows only (Sort -> scan over the
    /// test tuple); the nodes above it take the rechecked row as any other, as the IR recheck's row goes on through `k`'s
    /// steps above the lock, which read those products as they read every row's.
    pub(crate) fn above_feeds(&self, k: usize, j: usize, q: usize) -> usize {
        let gx = self.0;
        let ps = gx.pipelines();
        let p = &ps[k];
        let reads = |o: &Op<'_>, s: u8| match o {
            Op::Join(JoinDesc::Probe { slot, .. }) => *slot == s,
            Op::Join(JoinDesc::Nest { inner, .. }) => matches!(inner.source, Source::Breaker { slot } if slot == s),
            _ => false,
        };
        let feeds = |x: usize| {
            let Some(s) = ps[x].desc.produces().filter(|_| x != k && x != q && ps[x].desc.consumes.is_empty()) else { return false };
            let alone = ps.iter().enumerate().all(|(y, o)| y == k || !o.desc.consumes.contains(&s));
            let quiet = !ps[x].ops.iter().any(|o| matches!(o, Op::Lock(_) | Op::Write(_)));
            alone && quiet && !self.init_member(x) && !p.ops[..=j].iter().any(|o| reads(o, s)) && p.ops[j + 1..].iter().any(|o| reads(o, s))
        };
        (0..ps.len()).filter(|&x| feeds(x)).count()
    }

    /// D3563: the number of pipelines in the rerun sets of pipeline `k`'s SubPlan steps before op `j` (each re-run at every
    /// call, as D652-S1's: the redo evaluates the step again), none writing or locking; else none.
    pub(crate) fn tail_sets(&self, k: usize, j: usize) -> Option<usize> {
        let gx = self.0;
        let ps = gx.pipelines();
        let member = |q: usize| gx.graph.reruns.iter().any(|r| ps[k].ops[..j].iter().any(|o| matches!(o, Op::SubPlan(s) if s.plan_id == r.plan_id)) && r.pipelines.iter().any(|&m| usize::from(m) == q));
        let members: PgVec<'_, usize> = (0..ps.len()).filter(|&q| q != k && member(q)).collect_in(gx.mcx);
        let quiet = members.iter().all(|&q| !ps[q].ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))));
        quiet.then_some(members.len())
    }

    /// D3561 REFANTI (`breaker_limit`, 15b): the lock pipeline `k` alone with its Sort's producer `q` and the pipeline `extra`
    /// the producer's ANTI join reads (a hash build's, or a Materialize's producer), beside InitPlans that lock and write
    /// nothing and the pipelines only reading or feeding above the lock, counted as `Beside::Alone` counts them, plus `extra`.
    pub(crate) fn alone_with(&self, k: usize, j: usize, q: usize, extra: Option<usize>) -> bool {
        let gx = self.0;
        let ps = gx.pipelines();
        let n = self.inits(k, q);
        // D3563 (`sort_tail`): and the rerun sets of the Result's SubPlan steps (a correlated aggregate's), quiet
        let tail = match crate::ops::lockrows::lower::sort_tail(&ps[k], j) {
            true => self.tail_sets(k, j),
            false => Some(0),
        };
        n.zip(self.above_only(k)).zip(tail).is_some_and(|((n, a), t)| self.whole(2 + n + a + t + self.above_feeds(k, j, q) + usize::from(extra.is_some())))
    }

    /// D3277 (DRAINED LIMIT): `breaker_limit`'s lock (pipeline `k`, its producer `q`) beside data-modifying CTEs nothing reads
    /// (rerun/epq/marks.rs `drained`), every InitPlan a CTE's and every other pipeline theirs, the statement's one row lock its
    /// own (the CTE's write claims its plan beside the lock's only when it never stops for the runner's EvalPlanQual: D3276's
    /// rule, else the statement refuses there). PostgreSQL
    /// runs such a CTE once, at ExecPostprocessPlan, and its EvalPlanQual tree holds no CteScan of it: the recheck reruns
    /// Sort -> scan over the test tuple alone, as 15b's IR recheck over `q` does. The CTE's write is a plan beside the lock's.
    pub(crate) fn beside_drained(&self, k: usize, q: usize) -> bool {
        let gx = self.0;
        let ps = gx.pipelines();
        // D3379 LOCKPOST: or beside a post CTE (ops/modifytable/post.rs: one without RETURNING, run after the emit)
        let drained = gx.after_emit();
        let locks = ps.iter().chain(ps.inners().into_iter().map(|x| x.1)).flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
        !drained.is_empty()
            && !drained.contains(&k)
            && !drained.contains(&q)
            && !gx.cte_write(k)
            && !gx.cte_write(q)
            && self.whole(2 + drained.len())
            && locks == 1
            && self.ctes_only()
    }

    /// D3558: the `Program.locks` entry (its index + 1) of the CTE claim in pipeline `k` that `Plan::write_pair` admitted:
    /// its place among `cte_claims`, else 1 (the one claim).
    pub(crate) fn claim_entry(&self, k: usize, limit_op: fn(&Op<'_>) -> bool) -> usize {
        self.cte_claims(limit_op).and_then(|ks| ks.iter().position(|&x| x == k)).map_or(1, |i| i + 1)
    }

    /// D3506 SETLOCK: the `Program.locks` entry (1-based) of the set lock at op `j` of hashed SubPlan set build `k`, after
    /// the claims' (rerun/epq/set_lock.rs `mark`).
    pub(crate) fn set_entry(&self, k: usize, j: usize, limit_op: fn(&Op<'_>) -> bool) -> Option<usize> {
        let claims = self.cte_claims(limit_op).map_or(1, |ks| ks.len());
        crate::rerun::epq::set_lock::mark(self.0, k, j, claims)
    }

    /// The `Pipes` index of embedded inner `i`: the inners are numbered past the graph pipelines, in `Pipes::inners` order.
    pub(crate) fn inner_index(&self, i: usize) -> usize {
        self.0.pipelines().len() + i
    }
}

/// The lock plan's state in `Px`: D4154 INITMULTI, the M4 pair's B over several marks (its LockRows node), laid by
/// `Plan::role`; true once marks.rs `make` numbers its marks after A's entry (`Plan::runner`). D4157 INITSORT: or B over
/// a breaker. The last field: B's marks laid after A's (several), which no other form numbers, so unclaimed at the
/// program's end it refuses (`unclaimed`); B over a breaker alone keeps [A, B], as an AGA-35 bound form's B numbers it.
#[derive(Default)]
pub(crate) struct PlanState {
    multi: Option<(i32, bool, bool)>,
}

/// D4154 INITMULTI: whether the lock at plan node `node` is the M4 pair's B that `Plan::role` laid for marks.rs's runner.
pub(crate) fn multi_b(px: &Px<'_, '_>, node: i32) -> bool {
    px.lock_plan.multi.is_some_and(|(n, _, _)| n == node)
}

/// D4154 INITMULTI: 1 when the lock at plan node `node` is that B and no runner has numbered its marks yet (they go after
/// A's entry), else 0.
pub(crate) fn runner_off(px: &Px<'_, '_>, node: i32) -> usize {
    usize::from(px.lock_plan.multi.is_some_and(|(n, done, _)| n == node && !done))
}

/// At the program's end: B's marks laid after A's for marks.rs's runner that no runner numbered (another form made B, its
/// marks numbered from the first entry over A's) refuses at B's node (LOCK_INIT_UNCLAIMED).
pub(crate) fn unclaimed(px: &Px<'_, '_>) {
    if let Some((n, false, true)) = px.lock_plan.multi {
        shape(n, crate::ops::lockrows::lock_init::LOCK_INIT_UNCLAIMED);
    }
}

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

/// A lock op: its pipeline, its op, its desc and its LockRows node.
type At<'g, 'p> = (usize, usize, &'g LockDesc<'p>, i32);

/// C11 FORMS: a lock's form by name, in the order `Plan::form` tries them (the runner last).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FormId {
    Dummy,
    LockFill,
    SetLock,
    SubLock,
    ExistsSort,
    Sort15b,
    RefCopy,
    CteSort,
    JobqSort,
    JobqHash,
    RefFetch,
    MergeTail,
    RefChain,
    RefNest,
    RefNestChain,
    RefNestClaim,
    SortNest,
    SortNestCte,
    RefAnti,
    RefAntiCte,
    CopyProbe,
    CopyNest,
    LockNest,
    LockChain,
    LockPair,
    RefBuild,
    FamLimit,
    FamClaim,
    SortChain,
    RefDrain,
    Marks,
    Runner,
}

impl FormId {
    const ALL: [FormId; 32] = [FormId::Dummy, FormId::LockFill, FormId::SetLock, FormId::SubLock, FormId::ExistsSort, FormId::Sort15b, FormId::RefCopy, FormId::CteSort, FormId::JobqSort, FormId::JobqHash, FormId::RefFetch, FormId::MergeTail, FormId::RefChain, FormId::RefNest, FormId::RefNestChain, FormId::RefNestClaim, FormId::SortNest, FormId::SortNestCte, FormId::RefAnti, FormId::RefAntiCte, FormId::CopyProbe, FormId::CopyNest, FormId::LockNest, FormId::LockChain, FormId::LockPair, FormId::RefBuild, FormId::FamLimit, FormId::FamClaim, FormId::SortChain, FormId::RefDrain, FormId::Marks, FormId::Runner];

    /// Its shapecheck signal, `lock.<name>`.
    pub(crate) const fn fire(self) -> &'static str {
        match self {
            FormId::Dummy => "lock.dummy",
            FormId::LockFill => "lock.lock_fill",
            FormId::SetLock => "lock.set_lock",
            FormId::SubLock => "lock.sub_lock",
            FormId::ExistsSort => "lock.exists_sort",
            FormId::Sort15b => "lock.sort_15b",
            FormId::RefCopy => "lock.ref_copy",
            FormId::CteSort => "lock.cte_sort",
            FormId::JobqSort => "lock.jobq_sort",
            FormId::JobqHash => "lock.jobq_hash",
            FormId::RefFetch => "lock.ref_fetch",
            FormId::MergeTail => "lock.merge_tail",
            FormId::RefChain => "lock.ref_chain",
            FormId::RefNest => "lock.ref_nest",
            FormId::RefNestChain => "lock.ref_nest_chain",
            FormId::RefNestClaim => "lock.ref_nest_pair",
            FormId::SortNest => "lock.sort_nest",
            FormId::SortNestCte => "lock.sort_nest_cte",
            FormId::RefAnti => "lock.ref_anti",
            FormId::RefAntiCte => "lock.ref_anti_cte",
            FormId::CopyProbe => "lock.copy_probe",
            FormId::CopyNest => "lock.copy_nest",
            FormId::LockNest => "lock.lock_nest",
            FormId::LockChain => "lock.lock_chain",
            FormId::LockPair => "lock.lock_pair",
            FormId::RefBuild => "lock.ref_build",
            FormId::FamLimit => "lock.fam_limit",
            FormId::FamClaim => "lock.fam_claim",
            FormId::SortChain => "lock.sort_chain",
            FormId::RefDrain => "lock.ref_drain",
            FormId::Marks => "lock.marks",
            FormId::Runner => "lock.runner",
        }
    }

    const fn bit(self) -> u64 {
        1 << self as u32
    }
}

/// C11 REFINES edges: `Wins`, a lock both forms' tests accept takes `a`; `Declines`, `a`'s admission declines it and the
/// lock goes on to `b` (or past it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Wins,
    Declines,
}

/// C11 REFINES: `(a, b, edge, why)`, `a` tried before `b`, for PostgreSQL's reason `why`. Any other pair of forms accepting
/// one lock is an overlap, and so is a pair with the answer whose edge is not the one it took (`unordered`).
const REFINES: &[(FormId, FormId, Edge, &str)] = &[
    (FormId::ExistsSort, FormId::Marks, Edge::Wins, "the EXISTS claim over a Sort: its IR recheck redoes the producer's part with the SubPlan steps made again, as EvalPlanQual reruns the Sort's subtree"),
    (FormId::Sort15b, FormId::JobqHash, Edge::Wins, "JOBQ HASH's breaker test is 15b's without its census (the write's COPY mark is write_pair's question): a lock alone with its Sort's producer is 15b's"),
    (FormId::Sort15b, FormId::Marks, Edge::Wins, "under a LIMIT / OFFSET the runner's recheck refuses LIMIT_ABOVE; 15b's IR recheck reruns Sort -> scan over the test tuple and hands the row to the Limit, as EvalPlanQual and ExecLockRows do"),
    (FormId::RefCopy, FormId::Marks, Edge::Wins, "the Sort's producer nests over a COPY-marked row: the IR recheck hands the stored row to the inner (EvalPlanQualFetchRowMark's copy leg) and the row to the bound"),
    (FormId::CteSort, FormId::Marks, Edge::Wins, "a claim in a CTE beside the statement's write is a Program.locks entry of its own, rechecked as the CTE's LockRows is under its own EvalPlanQual; declined (no such write), the marks form"),
    (FormId::JobqSort, FormId::Marks, Edge::Wins, "the job-queue claim in an InitPlan beside the statement's write is a Program.locks entry of its own, the write's recheck copying the InitPlan's value; declined, the marks form"),
    (FormId::JobqHash, FormId::Marks, Edge::Wins, "the job-queue claim under the write's COPY mark is a Program.locks entry of its own, the write's recheck reading the subquery's row from that mark; declined, the marks form"),
    (FormId::RefFetch, FormId::Marks, Edge::Wins, "under a LIMIT the REFERENCE side's row is refetched at its junk ctid in the IR recheck (EvalPlanQualFetchRowMark) and the row handed to the Limit; the runner's recheck refuses LIMIT_ABOVE"),
    (FormId::MergeTail, FormId::Marks, Edge::Wins, "under a LIMIT the merge's REFERENCE inner is refetched in the IR recheck (EvalPlanQualFetchRowMark) and the row handed to the Limit"),
    (FormId::RefChain, FormId::Marks, Edge::Wins, "under a LIMIT each REFERENCE row of the chain is refetched in the IR recheck (EvalPlanQualFetchRowMark) and the row handed to the Limit"),
    (FormId::RefNest, FormId::RefNestClaim, Edge::Wins, "REFNEST's test is its claim form's without the write pair: a lock both accept is REFNEST's own Program.locks entry"),
    (FormId::SortNest, FormId::Marks, Edge::Wins, "a limited lock over a Sort of a REFERENCE nest: the IR recheck redoes REFNEST's inner over the Sort's producer and hands the row to the Limit"),
    (FormId::SortNestCte, FormId::Marks, Edge::Wins, "SORTNEST's claim in a CTE beside the statement's write is a Program.locks entry of its own; declined, the marks form"),
    (FormId::CopyNest, FormId::Marks, Edge::Wins, "a CTE lock over a nest of COPY rows: the IR recheck redoes the inner over the stored outer row, a Program.locks entry of its own beside the writes that read its store"),
    (FormId::LockNest, FormId::Marks, Edge::Wins, "a limited lock on a nest's inner: the IR recheck redoes the inner over the newest version joined to the outer row the nest holds and hands the row to the Limit"),
    (FormId::LockChain, FormId::Marks, Edge::Wins, "LOCKNEST's lock under a join of REFERENCE relations: the IR recheck reruns the outer row over the refetched rows (EvalPlanQualFetchRowMark) and hands the row to the Limit"),
    (FormId::LockPair, FormId::Marks, Edge::Wins, "both locks of a limited nest are Program.locks entries rechecked in the IR (pair.rs), each row handed on under the bound"),
    (FormId::RefBuild, FormId::Marks, Edge::Wins, "the locked relation on a hash build: the IR recheck redoes the build arm's part and refetches the REFERENCE probe row (EvalPlanQualFetchRowMark); the runner's recheck reruns no build"),
    (FormId::FamLimit, FormId::Marks, Edge::Wins, "a family's lock over a Sort of its Append under a LIMIT: the IR recheck redoes the arm the row's tableoid names and hands the row to the Limit; the runner's recheck refuses LIMIT_ABOVE"),
    (FormId::FamClaim, FormId::Marks, Edge::Wins, "a family's claim beside the statement's write is a Program.locks entry of its own over the arm the row's tableoid names; declined, the marks form"),
    (FormId::SortChain, FormId::Marks, Edge::Wins, "a lock behind a Sort of REFERENCE joins: the IR recheck redoes the source row and each join over the refetched rows and hands the row to the Limit"),
    (FormId::RefDrain, FormId::Marks, Edge::Wins, "a lock over the Sort of a Hash Right Anti Join's drain, the locked relation hashed: the runner's recheck reads the drain's product (D652); the IR recheck redoes the build's part over the newest version, the REFERENCE side empty (its junk ctid NULL in a drained row, so EvalPlanQualFetchRowMark finds no test tuple), then the drain's ops"),
];

/// C11 overlap: the forms whose shape test accepts the lock at op `j` of pipeline `k` (`d`), as `FormId` bits; LOCK-FILL
/// (a make-time answer) and the runner (no test of its own) aside.
pub(crate) fn accepts(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> u64 {
    use super::lower::{limit_op, sort_emit, Beside};
    use crate::rerun::epq::{ref_anti, ref_nest};
    let p = &gx.pipelines()[k];
    let tests = [
        (FormId::Dummy, crate::ops::shared::dummy::dummy(gx, k, j)),
        (FormId::SetLock, crate::rerun::epq::set_lock::form(gx, k, j).is_some()),
        (FormId::SubLock, crate::rerun::epq::sub_lock::form(gx, k, j).is_some()),
        (FormId::ExistsSort, crate::rerun::epq::sub_lock::sort_claim(gx, k, j).is_some()),
        (FormId::Sort15b, breaker_limit(gx, k, j, d, Beside::Alone).is_some()),
        (FormId::RefCopy, super::lower::ref_copy(gx, k, j, d).is_some()),
        (FormId::CteSort, breaker_limit(gx, k, j, d, Beside::Cte).is_some()),
        (FormId::JobqSort, breaker_limit(gx, k, j, d, Beside::Init).is_some()),
        (FormId::JobqHash, breaker_limit(gx, k, j, d, Beside::Copy).is_some()),
        (FormId::RefFetch, super::lower::ref_limit(gx, k, j, d).is_some()),
        (FormId::MergeTail, super::lower::merge_tail(gx, k, j, d).is_some()),
        (FormId::RefChain, super::lower::ref_chain(gx, k, j, d).is_some()),
        (FormId::RefNest, ref_nest::form(gx, k, j, d, limit_op, false).is_some()),
        (FormId::RefNestChain, ref_nest::chain_form(gx, k, j, d, limit_op).is_some()),
        (FormId::RefNestClaim, ref_nest::form(gx, k, j, d, limit_op, true).is_some()),
        (FormId::SortNest, ref_nest::sort_form(gx, k, j, d, (limit_op, sort_emit), false).is_some()),
        (FormId::SortNestCte, ref_nest::sort_form(gx, k, j, d, (limit_op, sort_emit), true).is_some()),
        (FormId::RefAnti, ref_anti::form(gx, k, j, d, limit_op, false).is_some()),
        (FormId::RefAntiCte, ref_anti::form(gx, k, j, d, limit_op, true).is_some()),
        (FormId::CopyProbe, crate::rerun::epq::ref_copy::probe_form(gx, k, j, d).is_some()),
        (FormId::CopyNest, ref_nest::copy_nest_form(gx, k, j, d).is_some()),
        (FormId::LockNest, ref_nest::lock_form(gx, k, j, d, limit_op).is_some()),
        (FormId::LockChain, ref_nest::chain_lock_form(gx, k, j, d, limit_op).is_some()),
        (FormId::LockPair, ref_nest::pair_form(gx, k, j, d, limit_op).is_some()),
        (FormId::RefBuild, super::lower::ref_build(gx, k, j, d).is_some()),
        (FormId::FamLimit, super::lower::family_limit(gx, k, j, d, false).is_some()),
        (FormId::FamClaim, super::lower::family_limit(gx, k, j, d, true).is_some()),
        (FormId::SortChain, crate::rerun::epq::sort_chain::form(gx, k, j, d, (limit_op, sort_emit)).is_some()),
        (FormId::RefDrain, ref_drain(gx, k, j, d).is_some()),
        (FormId::Marks, super::marks::several(p, d) || super::marks::over_breaker(p, d) || super::marks::nest_outer(gx, k, d) || super::marks::family_breaker(p, d)),
    ];
    tests.iter().filter(|t| t.1).fold(0, |m, t| m | t.0.bit())
}

/// C11 overlap: the pairs of forms in `set` and the answer `id` that no row of `refines` orders, each `(a, b)` with `a` tried
/// first: a pair with the answer needs its edge (`a` the answer: it `Wins`; `b` the answer: `a` `Declines`), any other pair
/// either edge.
fn unordered<'r>(set: u64, id: FormId, refines: &'r [(FormId, FormId, Edge, &'r str)]) -> impl Iterator<Item = (FormId, FormId)> + 'r {
    let set = set | id.bit();
    let ids = FormId::ALL.into_iter().filter(move |f| set & f.bit() != 0);
    let need = move |a: FormId, b: FormId| if a == id { Some(Edge::Wins) } else if b == id { Some(Edge::Declines) } else { None };
    ids.clone().flat_map(move |a| ids.clone().filter(move |b| (a as u32) < (*b as u32)).map(move |b| (a, b))).filter(move |&(a, b)| !refines.iter().any(|r| r.0 == a && r.1 == b && need(a, b).is_none_or(|e| e == r.2)))
}

/// C11 overlap: `None` when the forms whose tests accept a lock (`accepts`) and its answer `id` are one form or a chain under
/// REFINES; else the Bug text naming every pair no row orders and the answer, `lock-forms: <a>/<b> .. answered <id>`.
fn overlap(set: u64, id: FormId) -> Option<&'static str> {
    let mut pairs = unordered(set, id, REFINES).peekable();
    pairs.peek()?;
    let names: Vec<String> = pairs.map(|(a, b)| format!("{}/{}", &a.fire()[5..], &b.fire()[5..])).collect(); // alloc: Cold (a debug build's refusal)
    Some(Box::leak(format!("lock-forms: {} accept one lock outside REFINES, answered {} (C11)", names.join(" "), &id.fire()[5..]).into_boxed_str())) // alloc: Cold (a debug build's refusal)
}

/// C11 FORMS: the REFERENCE / COPY rows a form's recheck refetches, (pipeline, junk ctid column, relation) each.
pub(crate) enum Fetch<'g> {
    None,
    One((usize, i16, u32)),
    Many(PgVec<'g, (usize, i16, u32)>),
}

impl Fetch<'_> {
    pub(crate) fn rows(&self) -> &[(usize, i16, u32)] {
        match self {
            Fetch::None => &[],
            Fetch::One(f) => core::slice::from_ref(f),
            Fetch::Many(v) => v,
        }
    }
}

/// C11 FORMS: a lock op's answer (`Plan::form`), each the maker lower.rs make() calls with what its shape test returned
/// and the lock's Program.locks entry (`mark`).
pub(crate) enum Form<'g> {
    NoRows,
    SetLock(LockSite),
    SubLock(crate::rerun::epq::sub_lock::SubLock),
    SortSubClaim { at: (usize, usize), col: i16 },
    Breaker { at: (usize, usize), col: i16, fetch: Fetch<'g>, mark: usize },
    Tail { at: (usize, usize), col: i16, fetch: Fetch<'g>, mark: usize },
    RefNest { ctid: i16, fetch: Fetch<'g>, mark: usize },
    Probe { col: i16, mark: usize },
    LockNest { at: (usize, usize), ctid: i16, mark: usize },
    Pair((usize, usize, usize, usize, Option<(usize, usize)>)),
    RefBuild(crate::ops::lockrows::lower::RefBuild<'g>, usize),
    Family { arms: PgVec<'g, (usize, usize, u32)>, ctid: i16, toid: i16, mark: usize },
    SortChain(crate::rerun::epq::sort_chain::SortChain<'g>),
    Drain(RefDrain, usize),
    Marks { fam: bool },
    Runner(RunnerAnswer),
}

/// C11 FORMS: the runner form's decisions (lower.rs make() builds it): the lock's Program.locks entry, a write pair's
/// claim, an EXISTS claim beside an inner lock, and its recheck in the IR as a product (`product`) or over SubPlan steps
/// (`subs`; `twin`: the redo probing sets of its own).
pub(crate) struct RunnerAnswer {
    pub(crate) mark: usize,
    pub(crate) pair: bool,
    pub(crate) sub_claim: bool,
    pub(crate) product: bool,
    pub(crate) subs: bool,
    pub(crate) twin: bool,
}

/// The lock plan's decisions that write `Px`: a lock's role (the M4 pair's A or B, a claim beside the statement's write, a
/// set's lock) and the `Program.locks` entries it lays. Every write of `Px::locks` is one of these.
pub(crate) struct Plan<'a, 'g, 'p>(pub(crate) &'a mut Px<'g, 'p>);

impl<'g> Plan<'_, 'g, '_> {
    /// M4 (lock_init.rs module doc): pipeline `k`'s lock's index in `Program.locks` + 1 (A 1, B 2 for each of B's arms: its
    /// first non-parent locking mark's LockSite), both entries laid in `Px::locks`; 0 for a program that is not the pair
    /// (its one lock plan, or nest.cte's refusal at the second). Its site tests: lower.rs `one_site` (V4R-512's product
    /// test) and marks.rs `several` (Row A's multilock test); a refusal is at `node`, the asking lock's.
    pub(crate) fn role(&mut self, k: usize, node: i32) -> usize {
        let px = &mut *self.0;
        let (one_site, several) = (crate::ops::lockrows::lower::one_site, crate::ops::lockrows::marks::several);
        let gx = px.gx;
        let ps = gx.pipelines();
        let mut locks: PgVec<'_, At<'_, '_>> = PgVec::new_in(gx.mcx);
        for (q, p) in ps.iter().enumerate() {
            for (j, o) in p.ops.iter().enumerate() {
                let Op::Lock(d) = o else { continue };
                if !crate::ops::shared::dummy::dummy(gx, q, j) {
                    locks.push_in((q, j, d, p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id)));
                }
            }
        }
        let mut nodes: PgVec<'_, i32> = locks.iter().map(|l| l.3).collect_in(gx.mcx);
        nodes.sort_unstable();
        nodes.dedup();
        // D3660: beside one lock node alone among the graph pipelines, B may be a lock in an embedded inner (lockrows SUBLOCK's
        // SubPlan lock, rerun/epq/sub_lock.rs `form`), numbered past the graph pipelines as `Pipes::inners` orders them
        let (n, inners) = (ps.len(), ps.inners());
        if nodes.len() == 1 {
            for (x, (_, ip)) in inners.iter().enumerate() {
                for (j, o) in ip.ops.iter().enumerate() {
                    if let Op::Lock(d) = o {
                        locks.push_in((n + x, j, d, ip.node_ops.iter().rev().find(|m| m.first_op as usize <= j).map_or(ip.node_id, |m| m.node_id)));
                    }
                }
            }
            nodes = locks.iter().map(|l| l.3).collect_in(gx.mcx);
            nodes.sort_unstable();
            nodes.dedup();
        }
        let pipe = |q: usize| if q < n { &ps[q] } else { inners[q - n].1 };
        // A: the lock node one of whose ops an InitPlan's pipelines run (its arms with it); B: the other, none inside one
        // D3718 LOCK-FILL: or A the lock in a nest's plan Material fill, pulled by its reader (D4204) or run whole at the nest's first outer row
        // (rerun/spool/form.rs `lock_fill`), B the root's lock before that nest (PostgreSQL: two LockRows, each its own
        // ExecRowMarks and EvalPlanQual over its own subtree)
        let init = Census(gx).init_pipelines();
        let fill = |q: usize| q < n && crate::rerun::spool::form::lock_fill_of(gx, q);
        // D3775 COPYPAIR: or A the lock below the copy entry of a SubqueryScan whose row the other node's COPY mark carries,
        // in a pipeline of its own making a product (SQLAlchemy's joinedload of a `LIMIT n FOR UPDATE` subquery under the
        // statement's FOR UPDATE, the subquery a hash build): B's recheck reads that row stored, never running A again
        let copied = |l: &&At<'_, '_>| l.0 < n && ps[l.0].desc.produces().is_some() && locks.iter().any(|o| o.3 != l.3 && o.0 != l.0 && below_copy(gx, l.0, l.1, &o.2.marks));
        let Some(na) = locks.iter().find(|l| init.contains(&l.0)).or_else(|| locks.iter().find(|l| fill(l.0))).or_else(|| locks.iter().find(copied)).map(|l| l.3).filter(|_| nodes.len() == 2) else { return 0 };
        let (mut a, mut b): (PgVec<'_, At<'_, '_>>, PgVec<'_, At<'_, '_>>) = (PgVec::new_in(gx.mcx), PgVec::new_in(gx.mcx));
        for &l in locks.iter() {
            match l.3 == na {
                true => a.push_in(l),
                false => b.push_in(l),
            }
        }
        let (Some(&(_, _, da, _)), Some(&(qb, jb, db, nb))) = (a.first(), b.first()) else { return 0 };
        if b.iter().any(|l| init.contains(&l.0) || l.0 < n && ps[l.0].desc.produces().is_some() || a.iter().any(|m| m.0 > l.0)) || b.len() > 1 && db.family().is_none() {
            return 0;
        }
        // D4150 INITFAM (the temporal-FK RI restrict check over a partitioned or inheritance PK table): or A a family's lock,
        // each of its arms (or the planner's one leaf) over its own leaf's scan making the InitPlan's product (lower.rs
        // `own_leaf`, producerchain E): the arms share A's entry, each on its own late lock site over its leaf (B's family
        // form), and each rechecks that leaf's newest version in the IR on the way to the product, as PostgreSQL's LockRows
        // over the Append rechecks the row's leaf (the other children's marks inactive) and its Aggregate reads the row
        if a.len() > 1 && da.family().is_none() || a.iter().any(|l| l.0 >= n) && da.family().is_some() {
            shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_FAMILY);
        }
        // D3657: B in REFBUILD's form (lower.rs `ref_build`) is one entry: its one locking mark is on the build side of the join
        // its pipeline probes, never its pipeline's own source. D4154 INITMULTI: B of several marks over a join (no OF, or OF x
        // beside a REFERENCE relation) is marks.rs's form, the runner's recheck with the InitPlan's value copied: its marks are
        // Program.locks entries after A's (`PlanState`, claimed by marks.rs `make`); B in an embedded inner is not
        // D4157 INITSORT: so is B over a breaker (an ORDER BY under it, marks.rs `over_breaker`; a family's over its Append,
        // `family_breaker`), unless an AGA-35 bound form (D3436: 15b, FAMLIMIT, REFFETCH under a LIMIT) makes it first
        let sorted = |p: &PipelineNode<'_>| crate::ops::lockrows::marks::over_breaker(p, db) || crate::ops::lockrows::marks::family_breaker(p, db);
        let several_b = qb < n && several(pipe(qb), db);
        let multi = qb < n && (several_b || sorted(&ps[qb])) && crate::ops::lockrows::lower::ref_build(gx, qb, jb, db).is_none();
        if a.iter().any(|l| several(pipe(l.0), l.2)) || qb >= n && several(pipe(qb), db) {
            shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_MULTI);
        }
        // D3379 INITSINK: or A's pipeline is its InitPlan's own, ending in the sink that binds the Init's params (an EXPR,
        // MULTIEXPR or ROWCOMPARE sublink's `(SELECT x .. FOR UPDATE)`, which PostgreSQL's ExecSetParamPlan reads to its end
        // or its second row's raise): the rechecked row goes on to the sink as to a product (`Plan::form`, `mark` 1)
        // D4150 INITFAM: a family's arms each make the product over its own leaf (`own_leaf`, no sink form)
        let form = |&(q, j, d, _): &At<'_, '_>| match d.family() {
            None => (ps[q].desc.produces().is_some() || super::lock_init::init_sink(gx, q)) && one_site(&ps[q], j, d),
            Some(_) => ps[q].desc.produces().is_some() && crate::ops::lockrows::lower::own_leaf(&ps[q], j, d),
        };
        if !a.iter().all(form) {
            shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_FORM);
        }
        if ps.iter().chain(ps.inners().into_iter().map(|x| x.1)).flat_map(|p| p.ops.iter()).any(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select)) {
            shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_WRITE);
        }
        let sites = |d: &LockDesc<'_>| d.marks.iter().filter(|m| m.lock != LockKind::None && !m.parent).map(|m| LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait }).collect_in(gx.mcx);
        let (sa, sb) = (sites(da), sites(db));
        let (Some(&a1), Some(&b1)) = (sa.first(), sb.first()) else { crate::refuse!(node, Reason::Bug { payload: "a LockRows of the InitPlan pair without a locking mark" }) };
        // D4154 INITMULTI: B's every locking mark in marks.rs's order (PlanRowMark order), whichever of the two makes is first;
        // D4157: B over a breaker alone keeps [A, B], the entries an AGA-35 bound form's B (plan.rs `Plan::pair_entry`) has too, and
        // marks.rs `make` lays its own after A's when it claims B
        let laid = multi && several_b;
        px.locks = match laid {
            true => core::iter::once(a1).chain(sb).collect(), // alloc: Program
            false => vec![a1, b1], // alloc: Program
        };
        if multi && px.lock_plan.multi.is_none() {
            px.lock_plan.multi = Some((nb, false, laid));
        }
        if a.iter().any(|l| l.0 == k) { 1 } else if b.iter().any(|l| l.0 == k) { 2 } else { 0 }
    }

    /// D3379 INITSINK: the `Program.locks` entry of an AGA-35 form's lock at plan node `node` (pipeline `k`, its rechecked
    /// producer `q`): 2 beside the InitPlan whose own row lock is the M4 pair's A (`Census::pair_inits`; `role` asks A's
    /// form and lays both entries), else 0 (the program's one lock). PostgreSQL's InitPlan LockRows rechecks under its own
    /// EvalPlanQual inside the InitPlan's run; this lock's EvalPlanQual copies the InitPlan's value (EvalPlanQualStart).
    pub(crate) fn pair_entry(&mut self, k: usize, q: usize, node: i32) -> usize {
        if Census(self.0.gx).pair_inits(k, q).is_none() {
            return 0;
        }
        let mark = self.role(k, node);
        if mark != 2 {
            shape(node, crate::ops::lockrows::lock_init::LOCK_INIT_FORM);
        }
        mark
    }

    /// JOBQ (lk-0055 / 0058 / 0063 / 0066; IN form lk-0056 / 0059 / 0064 / 0067): the job-queue claim `UPDATE /
    /// DELETE t .. WHERE id = (SELECT id FROM t .. ORDER BY .. LIMIT 1 FOR UPDATE [SKIP LOCKED])` (or `id IN (..LIMIT n..)`):
    /// the program's one lock, at pipeline `k`'s op `j`, is under a LIMIT / OFFSET (`limit`), over its own relation alone
    /// (`one_site`), inside a non-CTE InitPlan's members or below the SubqueryScan whose row the write's COPY mark carries,
    /// and the program's other row op is the statement's own UPDATE / DELETE (outside every InitPlan and CTE). PostgreSQL runs the InitPlan once; its
    /// LockRows keeps its own ExecRowMark and rechecks under its own EvalPlanQual (the Sort-free subtree over the test tuple,
    /// LockRows handing the row to the Limit), and the write's recheck copies the InitPlan's value (EvalPlanQualStart). Here
    /// the lock is a `Program.locks` entry of its own (its own lock host), rechecked in the IR (lower.rs), and the write keeps
    /// the program's `write` plan. The lock's site once served (the caller's lock mark 1), else false.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_pair(&mut self, k: usize, j: usize, node: i32, d: &LockDesc<'_>, limit: bool, one_site: fn(&PipelineNode<'_>, usize, &LockDesc<'_>) -> bool, limit_op: fn(&Op<'_>) -> bool) -> bool {
        let px = &mut *self.0;
        let gx = px.gx;
        let ps = gx.pipelines();
        let init = Census(gx).init_pipelines();
        if !one_site(&ps[k], j, d) {
            return false;
        }
        // the program's one LockRows (a second one, even over the planner's dummy relation, keeps the pair forms above); D3558:
        // or several claims, each inside its own CTE (`cte_claims`)
        let several = Census(gx).cte_claims(limit_op);
        // D3506 SETLOCK: a lock in a hashed SubPlan's plan (rerun/epq/set_lock.rs) is a Program.locks entry of its own
        // after the claims', no claim
        let set_locks = crate::rerun::epq::set_lock::locks(gx);
        let not_set = |q: usize, i: usize| !set_locks.iter().any(|x| x.0 == q && x.1 == i);
        let nlocks = ps.iter().enumerate().flat_map(|(q, p)| p.ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Lock(_))).map(move |(i, _)| (q, i))).filter(|&(q, i)| not_set(q, i)).count();
        if nlocks != 1 && several.is_none() {
            return false;
        }
        let own = |q: usize| !init.contains(&q) && !gx.cte_write(q);
        let writes = ps.iter().enumerate().flat_map(|(q, p)| p.ops.iter().map(move |o| (q, o))).filter(|(_, o)| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select));
        // CTELOCK: the lock inside a CTE that writes nothing, its store read whole by the statement: each reader scans the
        // store as its source (no nest inner), and no LIMIT / OFFSET outside the CTE stops a read early (PostgreSQL's
        // CteScan pulls the CTE's LockRows only as far as its readers read); the CTE's product is the lock's (a
        // product-form lock, rechecked in the IR); an UPDATE / DELETE's recheck reads the CteScan's stored row
        // (ROW_MARK_COPY, copy_row.rs: the CTE never reruns), an INSERT never rechecks. CTELIMIT (ops/shared/limit.rs
        // `cte_lock_cap`): or read by the statement's INSERT under a LIMIT of constants, the CTE's own Limit capped at the
        // rows that INSERT reads
        let in_cte = Census(gx).cte_whole(k, limit_op);
        let (mut n, mut main) = (0, None);
        // D3379 FAMCLAIM HASH: the statement's one UPDATE / DELETE over a partitioned target may be one write op per leaf, each
        // in a union arm (the leaf's scan probing the hashed subquery): every one the same command and root
        let mut arms: PgVec<'_, &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>> = PgVec::new_in(gx.mcx);
        // D3488 CTECLAIM: or one data-modifying CTE's UPDATE / DELETE (its CTE's own pipeline) whose COPY mark carries the
        // claim (`below_copy`), every other write a plain INSERT: the archive or log of the claimed rows, `WITH d AS (DELETE
        // FROM job WHERE id IN (SELECT id .. LIMIT n FOR UPDATE SKIP LOCKED) RETURNING ..) INSERT INTO done SELECT * FROM d`.
        // That write rechecks in its own IR over the claim's build (rowop_epq.rs `claim_built`), never running the subquery
        // again, as PostgreSQL's EvalPlanQual hands the SubqueryScan the stored row
        let (mut cte, mut ncte, mut inserts) = (None, 0, 0);
        let mut cte_writes: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
        for (q, o) in writes {
            n += 1;
            if let Op::Write(w) = o {
                if own(q) && (matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete) || in_cte && w.cmd == gravity_program::Cmd::Insert) {
                    main = Some(w);
                }
                if own(q) && matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete) && ps[q].ops.iter().any(crate::graph::is_arm) {
                    arms.push_in(w);
                }
                if gx.cte_write(q) && !init.contains(&q) && matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete) {
                    cte = Some((q, w));
                    ncte += 1;
                    cte_writes.push_in(q);
                }
                if w.cmd == gravity_program::Cmd::Insert && w.on_conflict.is_none() && w.merge.is_none() {
                    inserts += 1;
                }
            }
        }
        let one_root = arms.len() == n && n > 1 && arms.iter().all(|w| w.cmd == arms[0].cmd && w.plan.target_rel == arms[0].plan.target_rel);
        // D3601 (4) (River's completer: job_to_update's FOR UPDATE read whole by updated_running and
        // updated_metadata_only): or several such writes beside CTELOCK's claim (`in_cte`) and no statement write, each
        // rechecking in its own IR
        let several_ctes = in_cte && ncte > 1 && gx.ctes_beside(&cte_writes[..]);
        let claim = cte.filter(|_| main.is_none() && (ncte == 1 || several_ctes) && ncte + inserts == n);
        // D3489 CTECLAIM-INIT: or that write reads the claim through an InitPlan at its ModifyTable its first recheck runs again
        // (`.. WHERE id = (SELECT id .. LIMIT 1 FOR UPDATE SKIP LOCKED)`: rerun/epq/inits.rs `cte_write_armed`), whose pipelines
        // hold this lock: the lock's Program.locks entry locks again in that run (its own lock TM_Ok)
        let claim_init = claim.is_some_and(|(q, _)| {
            let jq = ps[q].ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(ps[q].ops.len());
            crate::rerun::epq::inits::cte_write_armed(gx, q, jq).into_iter().any(|x| gx.with_producers([gx.graph.init_plans[x].pipeline as usize]).contains(&k))
        });
        let claim = claim.map(|(_, w)| w);
        // D3507 (graphile-worker's get_job: `WITH j AS (SELECT .. LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE job_queues
        // .. FROM j ..) UPDATE jobs .. FROM j ..`): or CTELOCK's claim beside the statement's UPDATE / DELETE and
        // data-modifying CTEs' UPDATE / DELETEs that each recheck in their own IR, every other write a plain INSERT
        // (`ctes_beside`)
        let beside = in_cte && main.is_some_and(|w| matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete)) && gx.ctes_beside(&cte_writes[..]) && 1 + ncte + inserts == n;
        let Some(w) = main.filter(|_| n == 1 || one_root || beside).or(claim) else { return false };
        // the claim's IN form (lk-0056 / 0059 / 0064 / 0067: `.. WHERE id IN (SELECT id .. LIMIT n FOR UPDATE ..)`, a
        // HashAggregate over the subquery): the lock is below the SubqueryScan whose row the write's ROW_MARK_COPY mark
        // carries, in the pipeline running it; the write's recheck enters that pipeline at the SubqueryScan with the stored row
        // (copy_row.rs), never at the lock (PG hands EvalPlanQual the stored row and never runs the subquery's LockRows again)
        let below_copy = below_copy(gx, k, j, w.mark_descs);
        // D3458 CLAIMALL (m4_iso: `DELETE FROM iso_job WHERE id IN (SELECT id FROM iso_job WHERE owner = 'A' FOR UPDATE NOWAIT)`,
        // a Hash Semi Join over the hashed SubqueryScan -> LockRows -> scan): the IN form needs no LIMIT. Its lock's pipeline
        // runs once into the product the write reads (the hash build), as PostgreSQL's Hash reads the subquery's LockRows to
        // its end once; the write's recheck reads the subquery's row from its COPY mark either way. The InitPlan form keeps
        // the LIMIT: an expression sublink's InitPlan reads its LockRows past the first row (ExecSetParamPlan's second-row
        // raise, D3379 INITSINK's pair)
        // D3558: or that data-modifying CTE's write reads the claim's store whole (CTELOCK's form beside a CTE's write, PgQueuer's
        // `updated AS (UPDATE .. WHERE id = ANY (SELECT id FROM combined_jobs) ..)`): its recheck reads each CTE Scan's row from
        // its COPY mark and never runs the claim again
        if !(limit && init.contains(&k) || below_copy) && !in_cte || claim.is_some() && !(below_copy || limit && claim_init || in_cte) {
            return false;
        }
        let site = |d: &LockDesc<'_>| d.marks.iter().find(|m| m.lock != LockKind::None && !m.parent).map(|m| LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait });
        px.locks = match several {
            Some(ks) => ks.iter().filter_map(|&x| ps[x].ops.iter().find_map(|o| if let Op::Lock(dx) = o { site(dx) } else { None })).collect(), // alloc: Program
            None => site(d).into_iter().collect(), // alloc: Program
        };
        let claims = px.locks.len();
        px.locks.extend(set_locks.iter().map(|x| x.2)); // alloc: Program
        claims > 0
    }

    /// D3506 SETLOCK: entry `mark` (1-based) is `site`, the entries before it padded with it until their own lock lays
    /// them (a set's lock after the claims', `Census::set_entry`).
    pub(crate) fn lay_at(&mut self, mark: usize, site: LockSite) {
        let px = &mut *self.0;
        if px.locks.len() < mark {
            px.locks.resize(mark, site); // alloc: Program
        }
        px.locks[mark - 1] = site;
    }

    /// A form's own entries (SUBLOCK's, LOCKPAIR's).
    pub(crate) fn lay(&mut self, locks: Vec<LockSite>) {
        self.0.locks = locks;
    }

    /// marks.rs's runner form: its locking marks' entries, after A's when `off` (`runner_off`) is 1, which numbers B.
    pub(crate) fn runner(&mut self, off: usize, locks: Vec<LockSite>) {
        let px = &mut *self.0;
        px.locks = px.locks.first().copied().filter(|_| off == 1).into_iter().chain(locks).collect(); // alloc: Program
        if let Some(m) = px.lock_plan.multi.as_mut().filter(|_| off == 1) {
            m.1 = true;
        }
    }

    /// C11 FORMS: the form of the lock at op `j` of pipeline `k` (`d`, its LockRows `node`); lower.rs make() builds it. Each
    /// answer fires its shapecheck signal; a debug build refuses an overlap (`overlap`) by Bug name.
    pub(crate) fn form(&mut self, k: usize, j: usize, node: i32, d: &LockDesc<'_>) -> Form<'g> {
        let (id, form) = self.chain(k, j, node, d);
        fires::fire(id.fire()); // shapecheck fire signal (C11): the lock's form
        if cfg!(debug_assertions) {
            if let Some(why) = overlap(accepts(self.0.gx, k, j, d), id) {
                crate::refuse!(node, Reason::Bug { payload: why });
            }
        }
        form
    }

    /// C11 FORMS: `form`'s chain, its answer named (`FormId`): the first form that serves the lock, each shape test in
    /// its order and a match its admission declines (a `write_pair` false) going on to the next; the chain's admissions,
    /// fires and Program.locks entries run here in that order. The only place the order lives.
    fn chain(&mut self, k: usize, j: usize, node: i32, d: &LockDesc<'_>) -> (FormId, Form<'g>) {
        let px = &mut *self.0;
        let p = &px.gx.pipelines()[k];
        // V4R-255 (D717): over the planner's dummy relation (dummy.rs: every leaf pruned, a constant-false qual) no
        // row reaches the lock, which takes no site and no write plan: the rows pass on (a parent's mark alone leaves
        // ExecLockRows' lr_arowMarks empty, and the one-time gate passes none)
        if crate::ops::shared::dummy::dummy(px.gx, k, j) {
            return (FormId::Dummy, Form::NoRows);
        }
        // D4204: a LOCK-FILL child its Material's reader pulls makes its lock there (ops/material/lower.rs `make_reader`); the
        // fill pipeline's own function is dead code (its end leaves at the open), so its lock takes no site and no entry
        if px.making == (k, k) && crate::rerun::spool::form::lock_fill_pulled(px.gx, k) {
            return (FormId::LockFill, Form::NoRows);
        }
        // D3506 SETLOCK: a lock in a hashed SubPlan's plan, a Program.locks entry of its own (rerun/epq/set_lock.rs)
        if let Some(site) = crate::rerun::epq::set_lock::form(px.gx, k, j) {
            return (FormId::SetLock, Form::SetLock(site));
        }
        // SUBLOCK (D1018 sub-shape E): a lock inside a nested SubPlan's inner, rechecked in the IR (rerun/epq/sub_lock.rs)
        if let Some(s) = crate::rerun::epq::sub_lock::form(px.gx, k, j) {
            return (FormId::SubLock, Form::SubLock(s));
        }
        // D3506 EXISTS SORT: the claim over a Sort whose producer's filter runs that inner, its recheck redoing the
        // producer's part with the SubPlan steps made again for it (rerun/epq/sub_lock.rs `sort_claim`)
        if let Some((q, jq, col)) = crate::rerun::epq::sub_lock::sort_claim(px.gx, k, j) {
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
            return (FormId::ExistsSort, Form::SortSubClaim { at: (q, jq), col });
        }
        // CNM-38: a lock over a breaker (a Sort below LockRows) is marks.rs's form too: its row is its mark's junk ctid
        // lockrows 15b: under a LIMIT / OFFSET, a lock over a Sort of its own relation's one scan rechecks in the IR
        if let Some((q, jq, col)) = breaker_limit(px.gx, k, j, d, super::lower::Beside::Alone) {
            if Census(px.gx).beside_drained(k, q) {
                fires::fire("lock.drained_limit"); // shapecheck fire signal (D3277 DRAINED LIMIT): a 15b lock beside unread writing CTEs
            }
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
            let mark = Plan(px).pair_entry(k, q, node);
            // D3563: a Result's SubPlan steps past the Sort emit, redone after the producer's part
            if super::lower::sort_tail(&px.gx.pipelines()[k], j) {
                fires::fire("lock.sort_tail"); // shapecheck fire signal (D3563): a 15b lock past a Result's SubPlan over the Sort
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, k, j);
                return (FormId::Sort15b, Form::Tail { at: (q, jq), col, fetch: Fetch::None, mark });
            }
            return (FormId::Sort15b, Form::Breaker { at: (q, jq), col, fetch: Fetch::None, mark });
        }
        // D3569 REFCOPY: the same with the producer's nest over a COPY-marked subquery's row (Odoo's cron claim), its
        // stored row the inner's in the recheck (EvalPlanQualFetchRowMark's copy leg)
        if let Some((q, jq, col, fetch)) = super::lower::ref_copy(px.gx, k, j, d) {
            // AGA-35: the recheck redoes the producer's part (its nest read through its own expressions) and the subquery's
            // part from its entry over the stored row
            crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, q, jq);
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, fetch.0, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[fetch.0]));
            let mark = Plan(px).pair_entry(k, q, node);
            return (FormId::RefCopy, Form::Breaker { at: (q, jq), col, fetch: Fetch::One(fetch), mark });
        }
        // CTESORT: that lock inside a CTE that writes nothing, its store read whole by the
        // statement's one write (CTELOCK's form, plan.rs `Plan::write_pair`): a Program.locks entry of its own beside the write
        if let Some((q, jq, col)) = breaker_limit(px.gx, k, j, d, super::lower::Beside::Cte) {
            if Plan(px).write_pair(k, j, node, d, true, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir(px, k, node);
                // D3558: one of several CTE claims, its own Program.locks entry
                let mark = Census(px.gx).claim_entry(k, super::lower::limit_op);
                return (FormId::CteSort, Form::Breaker { at: (q, jq), col, fetch: Fetch::None, mark });
            }
        }
        // D3277 (JOBQ SORT): the job-queue claim whose subquery sorts (Limit -> LockRows -> Sort -> its relation's scan, no
        // index order) inside the InitPlan the statement's own UPDATE / DELETE reads: 15b's IR recheck over the Sort's producer,
        // as a Program.locks entry of its own beside the write (JOBQ's `write_pair`; the write's recheck copies the InitPlan's
        // value). PostgreSQL's InitPlan LockRows rechecks under its own EvalPlanQual: Sort -> scan over the test tuple, the
        // newest version through the scan's quals, then on to the Limit
        if let Some((q, jq, col)) = breaker_limit(px.gx, k, j, d, super::lower::Beside::Init) {
            if Plan(px).write_pair(k, j, node, d, true, super::lower::one_site, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
                return (FormId::JobqSort, Form::Breaker { at: (q, jq), col, fetch: Fetch::None, mark: 1 });
            }
        }
        // D3379 JOBQ HASH (m4_iso's claim): the same claim with the statement's own UPDATE / DELETE probing the hashed
        // subquery (Hash Semi Join: the target's scan probes Hash -> Subquery Scan -> Limit -> LockRows -> Sort -> scan), the
        // lock below the SubqueryScan whose row the write's ROW_MARK_COPY mark carries (`write_pair`'s IN form): 15b's IR
        // recheck over the Sort's producer, a Program.locks entry of its own beside the write. PostgreSQL's LockRows rechecks
        // under its own EvalPlanQual (Sort -> scan over the test tuple) and hands the row on through the Limit to the Hash; the
        // write's recheck reads the subquery's row from its COPY mark and never runs the LockRows again. The probe's
        // outer-first order (the Hash built at the target scan's first row) is the probe's own (hashjoin probe.rs, D824)
        if let Some((q, jq, col)) = breaker_limit(px.gx, k, j, d, super::lower::Beside::Copy) {
            if Plan(px).write_pair(k, j, node, d, true, super::lower::one_site, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
                return (FormId::JobqHash, Form::Breaker { at: (q, jq), col, fetch: Fetch::None, mark: 1 });
            }
        }
        // REFFETCH: the same over a Sort of a join whose other side is REFERENCE-marked, its
        // row refetched at its junk ctid in the recheck (PG's EvalPlanQualFetchRowMark)
        if let Some((q, jq, col, fetch)) = super::lower::ref_limit(px.gx, k, j, d) {
            // AGA-35: the recheck redoes the producer's part and the REFERENCE side's ops over its refetched row
            let pb = &px.gx.pipelines()[fetch.0];
            let jb = pb.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Build { .. }))).unwrap_or(pb.ops.len());
            crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, q, jq);
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, fetch.0, jb);
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::RefFetch, Form::Breaker { at: (q, jq), col, fetch: Fetch::One(fetch), mark });
        }
        // D3777 MERGETAIL: REFMERGE with the locked outer under a Sort, the recheck the Sort's producer's part, then the merge
        // pipeline's ops with the REFERENCE inner's row refetched
        if let Some((q, jq, col, fetch)) = super::lower::merge_tail(px.gx, k, j, d) {
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
            crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, k, j);
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, fetch.0, px.gx.pipelines()[fetch.0].ops.len());
            return (FormId::MergeTail, Form::Tail { at: (q, jq), col, fetch: Fetch::One(fetch), mark: 0 });
        }
        // D3567 REFCHAIN: the same over a chain of such joins (hash probes of REFERENCE builds, nests over REFERENCE
        // inners), each REFERENCE row refetched in the recheck
        if let Some((q, jq, col, fetches)) = super::lower::ref_chain(px.gx, k, j, d) {
            // AGA-35: the recheck redoes the producer's part (its joins read through their own expressions) and each
            // REFERENCE part over its refetched row
            crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, q, jq);
            crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, q, jq);
            for &(f, _, _) in &fetches {
                let pf = &px.gx.pipelines()[f];
                let end = pf.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Build { .. } | JoinDesc::Memo { .. }))).unwrap_or(pf.ops.len());
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, f, end);
            }
            let mark = Plan(px).pair_entry(k, q, node);
            return (FormId::RefChain, Form::Breaker { at: (q, jq), col, fetch: Fetch::Many(fetches), mark });
        }
        // REFNEST: under a LIMIT / OFFSET (or over a Memoize inner), a lock over an INNER nest whose inner is the
        // REFERENCE-marked relation's own scan, its row refetched in the IR's recheck under the nest's parameters
        if let Some(rn) = crate::rerun::epq::ref_nest::form(px.gx, k, j, d, super::lower::limit_op, false) {
            // AGA-35 form 5: the recheck redoes the outer part (the nest through its own expressions) and the REFERENCE
            // relation's part up to its Memoize or spool over the refetched row
            let q = rn.fetch.0;
            crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, k, j);
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[q]));
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::RefNest, Form::RefNest { ctid: rn.ctid, fetch: Fetch::One(rn.fetch), mark });
        }
        // D3564 REFNEST-CHAIN: the same over several such nests, INNER or LEFT, each REFERENCE row refetched in the recheck
        if let Some((col, fetches)) = crate::rerun::epq::ref_nest::chain_form(px.gx, k, j, d, super::lower::limit_op) {
            crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, k, j);
            for &(f, _, _) in &fetches {
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, f, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[f]));
            }
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::RefNestChain, Form::RefNest { ctid: col, fetch: Fetch::Many(fetches), mark });
        }
        // D3557: REFNEST's claim of the statement's own UPDATE / DELETE (the IN subquery its COPY mark carries, or its InitPlan):
        // a Program.locks entry of its own beside the write (JOBQ's forms, plan.rs `Plan::write_pair`)
        if let Some(rn) = crate::rerun::epq::ref_nest::form(px.gx, k, j, d, super::lower::limit_op, true) {
            if Plan(px).write_pair(k, j, node, d, true, |_, _, _| true, super::lower::limit_op) {
                let q = rn.fetch.0;
                crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, k, j);
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[q]));
                return (FormId::RefNestClaim, Form::RefNest { ctid: rn.ctid, fetch: Fetch::One(rn.fetch), mark: 1 });
            }
        }
        // D3554 SORTNEST: that nest below a Sort under a LIMIT / OFFSET, the recheck REFNEST's redo over the Sort's producer
        if let Some((q, jq, rn)) = crate::rerun::epq::ref_nest::sort_form(px.gx, k, j, d, (super::lower::limit_op, super::lower::sort_emit), false) {
            let f = rn.fetch.0;
            crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, q, jq);
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, f, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[f]));
            let mark = Plan(px).pair_entry(k, q, node);
            return (FormId::SortNest, Form::Breaker { at: (q, jq), col: rn.ctid, fetch: Fetch::One(rn.fetch), mark });
        }
        // D3555: that lock inside a CTE that writes nothing, its store read whole by the statement's one write (CTESORT's form):
        // a Program.locks entry of its own beside the write
        if let Some((q, jq, rn)) = crate::rerun::epq::ref_nest::sort_form(px.gx, k, j, d, (super::lower::limit_op, super::lower::sort_emit), true) {
            if Plan(px).write_pair(k, j, node, d, true, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir(px, k, node);
                return (FormId::SortNestCte, Form::Breaker { at: (q, jq), col: rn.ctid, fetch: Fetch::One(rn.fetch), mark: 1 });
            }
        }
        // D3562 REFANTI: a lock over an ANTI join whose inner reads a REFERENCE-marked relation's own scan (NOT EXISTS over a
        // side table), rechecked in the IR: no test tuple for that relation, so the newest version passes the join
        if let Some((ctid, probe)) = crate::rerun::epq::ref_anti::form(px.gx, k, j, d, super::lower::limit_op, false) {
            // AGA-35: the recheck redoes the lock pipeline's part, the join read through its own expressions
            match probe {
                true => crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, k, j),
                false => crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, k, j),
            }
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::RefAnti, Form::Breaker { at: (k, j), col: ctid, fetch: Fetch::None, mark });
        }
        // D3561: that lock inside a CTE that writes nothing (procrastinate's claim in its index order), its store read whole by
        // the statement's one write (CTELOCK's form): a Program.locks entry of its own beside the write
        if let Some((ctid, _)) = crate::rerun::epq::ref_anti::form(px.gx, k, j, d, super::lower::limit_op, true) {
            let limited = p.ops[j + 1..].iter().any(super::lower::limit_op);
            if Plan(px).write_pair(k, j, node, d, limited, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir(px, k, node);
                let mark = Census(px.gx).claim_entry(k, super::lower::limit_op);
                return (FormId::RefAntiCte, Form::Breaker { at: (k, j), col: ctid, fetch: Fetch::None, mark });
            }
        }
        // D3607 COPYPROBE (River's completer claim): a lock inside a CTE over INNER hash probes of COPY-marked rows (a
        // CTE's store, an inlined unnest subquery's rows), rechecked in the IR, each probe matched again over the record it
        // handed on: alone in the statement (its store read by the statement's SELECT), or a Program.locks entry of its own
        // beside the one write that reads its store (CTELOCK's form, plan.rs `Plan::write_pair`)
        if let Some(col) = crate::rerun::epq::ref_copy::probe_form(px.gx, k, j, d) {
            let ps = px.gx.pipelines();
            let locks = ps.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
            let writes = ps.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select)).count();
            let alone = locks == 1 && writes == 0;
            if alone || Plan(px).write_pair(k, j, node, d, false, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, k, j);
                let mark = if alone { 0 } else { Census(px.gx).claim_entry(k, super::lower::limit_op) };
                return (FormId::CopyProbe, Form::Probe { col, mark });
            }
        }
        // D3601 (6) COPYNEST (River's completer claim over its id list's Nested Loop): COPYPROBE's lock over a nest whose
        // outer is the COPY-marked rows and whose inner is the locked relation's scan, LOCKNEST's redo over the newest
        // version joined to the outer row the nest holds (the stored row EvalPlanQual hands the CTE Scan): alone in the
        // statement, or a Program.locks entry of its own beside the writes that read its store (plan.rs `Plan::write_pair`)
        if let Some((ctid, jn, ik)) = crate::rerun::epq::ref_nest::copy_nest_form(px.gx, k, j, d) {
            let ps = px.gx.pipelines();
            let locks = ps.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
            let writes = ps.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select)).count();
            let alone = locks == 1 && writes == 0;
            if alone || Plan(px).write_pair(k, j, node, d, false, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir_nest(px, k, node, k, j);
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, ik, crate::rerun::epq::ref_nest::inner_end(&px.gx.pipelines()[ik]));
                let mark = if alone { 0 } else { Census(px.gx).claim_entry(k, super::lower::limit_op) };
                return (FormId::CopyNest, Form::LockNest { at: (jn, ik), ctid, mark });
            }
        }
        // D3379 LOCKNEST (REFNEST's mirror): under a LIMIT / OFFSET, a lock on an INNER nest's inner relation beside
        // its REFERENCE-marked outer, the inner's redo over the newest version joined to the outer row the nest holds
        if let Some((ctid, jn, ik)) = crate::rerun::epq::ref_nest::lock_form(px.gx, k, j, d, super::lower::limit_op) {
            return (FormId::LockNest, Form::LockNest { at: (jn, ik), ctid, mark: 0 });
        }
        // D3780 LOCKCHAIN: LOCKNEST's lock under a join of REFERENCE relations (TypeORM's lock of a relation of a relation),
        // the outer row the nest holds the one EvalPlanQual's rerun over the refetched rows hands on
        if let Some((ctid, jn, ik)) = crate::rerun::epq::ref_nest::chain_lock_form(px.gx, k, j, d, super::lower::limit_op) {
            return (FormId::LockChain, Form::LockNest { at: (jn, ik), ctid, mark: 0 });
        }
        // D3379 LOCKPAIR: the same with the nest's outer relation locked too, both locks `Program.locks` entries (pair.rs)
        if let Some(f) = crate::rerun::epq::ref_nest::pair_form(px.gx, k, j, d, super::lower::limit_op) {
            return (FormId::LockPair, Form::Pair(f));
        }
        // REFBUILD: the locked relation (one family's leaves through their union, or one table under a LIMIT /
        // OFFSET) on the build side of the INNER hash join this pipeline probes, the probe side REFERENCE-marked
        if let Some(rb) = super::lower::ref_build(px.gx, k, j, d) {
            // AGA-35 form 3: the recheck redoes each build arm's part (and a family's union emit) over the locked row, then the
            // probe pipeline's part over the refetched REFERENCE row
            let (kp, jl) = rb.at.unwrap_or((k, j));
            crate::ops::lockrows::lock_init::admit_ir_ref(px, k, node, kp, jl);
            for &(q, jq, _) in &rb.arms {
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
            }
            if let Some((u, ju)) = rb.union {
                crate::ops::lockrows::lock_init::admit_ir_family(px, k, node, u, ju);
            }
            // D3777 LOCKMERGE: and a sorted merge outer's producer part, redone over the refetched row
            if let Some((q, jq)) = rb.so {
                crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, q, jq);
            }
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::RefBuild, Form::RefBuild(rb, mark));
        }
        // FAMLIMIT: the same over a Sort of one family's Append, the recheck over the arm the row's tableoid names
        if let Some((arms, ctid, toid)) = super::lower::family_limit(px.gx, k, j, d, false) {
            // AGA-35 form 4: the recheck redoes the arm the row's tableoid names, each arm's part asked on its own
            for &(q, jq, _) in &arms {
                crate::ops::lockrows::lock_init::admit_ir_family(px, k, node, q, jq);
            }
            let mark = Plan(px).pair_entry(k, k, node);
            return (FormId::FamLimit, Form::Family { arms, ctid, toid, mark });
        }
        // FAMCLAIM: that lock below the SubqueryScan whose row the statement's one write carries in its
        // ROW_MARK_COPY mark (JOBQ's IN form, plan.rs `Plan::write_pair`): a Program.locks entry of its own beside the write
        // D3502 FAMCTE: or inside a CTE that writes nothing, its store read whole by the statement's write (CTELOCK's form)
        if let Some((arms, ctid, toid)) = super::lower::family_limit(px.gx, k, j, d, true) {
            if Plan(px).write_pair(k, j, node, d, true, |_, _, _| true, super::lower::limit_op) {
                crate::ops::lockrows::lock_init::admit_ir(px, k, node);
                return (FormId::FamClaim, Form::Family { arms, ctid, toid, mark: 1 });
            }
        }
        // D3802 SORTCHAIN: LOCKCHAIN's lock behind a Sort (TypeORM's ordered find locking a relation of a relation), the
        // recheck EvalPlanQual's rerun redone whole: the producer's source row and each REFERENCE row refetched, the locked
        // inner's newest version, the joins over them (rerun/epq/sort_chain.rs)
        if let Some(sc) = crate::rerun::epq::sort_chain::form(px.gx, k, j, d, (super::lower::limit_op, super::lower::sort_emit)) {
            return (FormId::SortChain, Form::SortChain(sc));
        }
        // D3565 REFDRAIN: a lock over the Sort of a Hash Right Anti Join's drain, the locked relation the join's build
        // (REFBUILD's drain mirror, `ref_drain`): the recheck redoes the build's part over the newest version, the
        // probe side empty (its REFERENCE junk ctid NULL in a drained row), then the drain's ops up to the Sort
        if let Some(rd) = ref_drain(px.gx, k, j, d) {
            crate::ops::lockrows::lock_init::admit_ir_at(px, k, node, rd.build.0, rd.build.1);
            let mark = Plan(px).pair_entry(k, rd.q, node);
            return (FormId::RefDrain, Form::Drain(rd, mark));
        }
        // D2604 (marks.rs `make`): the family test its form starts from
        let fam = crate::ops::lockrows::marks::family_breaker(p, d);
        if crate::ops::lockrows::marks::several(p, d) || crate::ops::lockrows::marks::over_breaker(p, d) || crate::ops::lockrows::marks::nest_outer(px.gx, k, d) || fam {
            return (FormId::Marks, Form::Marks { fam });
        }
        // V4R-527 (M4): the RI temporal restrict check's two LockRows, each a Program.locks entry (0: the program's one lock)
        let mut mark = Plan(px).role(k, node);
        // JOBQ: the job-queue claim's lock under a LIMIT inside the InitPlan the statement's own UPDATE / DELETE reads
        // (or below the SubqueryScan its COPY mark carries; CTELOCK: inside a CTE the statement reads whole) is a Program.locks
        // entry of its own beside that write, rechecked in the IR as a product-form lock (plan.rs `Plan::write_pair`)
        // D3502 FAMCTE (index order): inside a CTE, or a family's lock over its own leaf's scan alone (`cte_site`)
        // D3506 SETLOCK: or a claim whose SubPlan steps below it all probe the sets of locks in hashed SubPlans' plans
        // (rerun/epq/set_lock.rs `probes_sets`): its own relation alone besides (`set_site`)
        let sets = crate::rerun::epq::set_lock::probes_sets(px.gx, k, j);
        let site: fn(&gravity_decompose::PipelineNode<'_>, usize, &LockDesc<'_>) -> bool = if sets { super::lower::set_site } else if px.gx.cte_write(k) { cte_site } else { super::lower::one_site };
        let pair = mark == 0 && Plan(px).write_pair(k, j, node, d, p.ops[j + 1..].iter().any(super::lower::limit_op), site, super::lower::limit_op);
        if pair {
            mark = Census(px.gx).claim_entry(k, super::lower::limit_op);
        }
        // D3506 SETLOCK: the locks in hashed SubPlans' plans are Program.locks entries after a write pair's claims
        if sets && !pair {
            shape(node, super::lower::SET_UNPAIRED);
        }
        // D3506 EXISTS: a claim beside an inner lock (rerun/epq/sub_lock.rs `sub_claim`) is Program.locks' entry 1, the
        // inner's 2 (`sub_lock`)
        let sub_claim = mark == 0 && !pair && crate::rerun::epq::sub_lock::sub_claim(px.gx, k, j);
        if sub_claim {
            mark = 1;
        }
        // D652 (D610's rest): the leader's recheck reruns the producers the stopped pipeline reads only when the other relations'
        // REFERENCE marks name them (D655, marks.rs: a lock over a join); any other product refuses by name
        // V4R-512 (D655's lock half, D1496's lock half): a lock over its own relation alone whose rows feed another pipeline's
        // product rechecks in the IR too (ExecLockRows hands its EPQ row to its parent): no other mark, nothing rerun
        // producerchain E: so does a family's lock (L2) whose pipeline makes another's product, each over its own leaf
        // D3379 INITSINK: so does the M4 pair's A (`mark` 1) whose pipeline ends in its InitPlan's sink (plan.rs `Plan::role`);
        // D3613: and the program's one lock in that place (`init_sink`), D652's checks then asking only the other InitPlans
        let sink = mark == 0 && !pair && crate::ops::lockrows::lock_init::init_sink(px.gx, k);
        let product = (p.desc.produces().is_some() || pair || mark == 1 || sink) && super::lower::one_site(p, j, d) || p.desc.produces().is_some() && super::lower::own_leaf(p, j, d);
        // D3379 OPABOVE: an op above the lock other than its projection and a LIMIT / OFFSET (the locked subquery's Subquery
        // Scan filter, `SELECT * FROM (.. LIMIT 5 OFFSET 2 ..) s WHERE s.id % 2 = 0 FOR UPDATE`): over its own relation alone
        // (`one_site`) the recheck runs in the IR as the product form's does, so the rechecked row goes on through those ops,
        // as ExecLockRows hands its EvalPlanQual row to its parent (the runner's recheck sends it to the client past them)
        let product = product || super::lower::one_site(p, j, d) && p.ops[j + 1..].iter().any(|o| !matches!(o, Op::Project(_)) && !super::lower::limit_op(o));
        // D652-S1's lock half: beside per-row SubPlan steps whose rerun sets are the only other pipelines, the IR's recheck too
        let subs = !product && super::lower::subs_in_ir(px.gx, k, j, d);
        // D3506 SETLOCK: or the claim beside the writes whose SubPlan steps probe the sets of locks in hashed SubPlans'
        // plans (rerun/epq/set_lock.rs): the redo probes sets of its own, as the LockRows' fresh EvalPlanQual estate builds
        // them
        let twin = !product && pair && sets;
        let subs = subs || twin;
        (FormId::Runner, Form::Runner(RunnerAnswer { mark, pair, sub_claim, product, subs, twin }))
    }
}

/// lockrows 15b (CNM-38 under a LIMIT / OFFSET): a lock over a Sort whose one input is its one locking mark's own heap scan
/// (only filters, gates and a final projection below the sort build; no other mark or pipeline; AGA-35: an InitPlan that
/// reads only, its pipelines counted out, `Census::plain_inits`, each one the recheck may read admitted by `admit_ir_at`;
/// INCAGG: the pipelines that only read the lock pipeline's product counted out too, `above_only`), with a LIMIT /
/// OFFSET above it. The runner's recheck (marks.rs's form) would send the rechecked row past the steps above (LIMIT_ABOVE);
/// PostgreSQL's EvalPlanQual reruns Sort -> scan over the test tuple alone and LockRows hands that row to the Limit. Here
/// the recheck runs in the IR over the producer's recheck quals, filters and projection (epq_ir.rs over pipeline `q`), whose
/// last projection is the row the Sort hands on, and the row goes on through the steps above, counted. The lock position
/// is the row's junk ctid. The lock's own pipeline holds only the Sort emit's projection below it (decompose's sort emit,
/// op 0), each column the producer's same column (OUTER_VAR, varattno = resno): the redo's last projection is then the row
/// the lock and the steps above read. Some((producer, its ops before the sort build, the junk ctid column)), else none.
/// The CTESORT form (`super::lower::Beside::Cte`): the lock pipeline is a CTE's store (its producer the Sort's), every InitPlan a CTE, the
/// other pipelines the statement's; `write_pair` then admits the write that reads the store. D3277 JOBQ SORT
/// (`super::lower::Beside::Init`): the lock pipeline is one a non-CTE InitPlan runs; `write_pair` then admits the statement's own write.
/// D3379 JOBQ HASH (`super::lower::Beside::Copy`): the lock pipeline is the statement's own, below the SubqueryScan whose row the
/// statement's own write carries in its COPY mark; `write_pair` admits that write (its `below_copy`).
fn breaker_limit(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, at: super::lower::Beside) -> Option<(usize, usize, i16)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    let alone = match at {
        // alone with its producer, or beside InitPlans that lock and write nothing (`Census::plain_inits`), their
        // pipelines counted out; `Plan::form` then asks `admit_ir_at` of each one the recheck may read
        // D3277 (DRAINED LIMIT, `beside_drained`): or beside data-modifying CTEs nothing reads, their pipelines counted out
        // INCAGG (D3409): the pipelines that only read the lock pipeline's product counted out too (`above_only`)
        // D3379 FEEDABOVE: and the pipelines that only feed an op above the lock (`above_feeds`), counted out too
        // D3379 INITSINK: or beside the InitPlan whose own row lock is the M4 pair's A (`Census::pair_inits`)
        // D3561 REFANTI: or one pipeline more, the one the producer's ANTI join reads (checked below, `alone_with`)
        super::lower::Beside::Alone => (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot)).is_some_and(|q| Census(gx).alone_with(k, j, q, None) || Census(gx).alone_with(k, j, q, Some(q)) || Census(gx).beside_drained(k, q)),
        super::lower::Beside::Cte => gx.cte_write(k) && Census(gx).ctes_only(),
        super::lower::Beside::Init => Census(gx).init_pipelines().contains(&k),
        // D3379 JOBQ HASH: the statement's own (no InitPlan's, no CTE's); `write_pair` then asks for the write's COPY mark
        // over the SubqueryScan above the lock
        super::lower::Beside::Copy => !gx.cte_write(k) && !Census(gx).init_pipelines().contains(&k),
    };
    // D3563: or (alone) the Sort emit's projection and a Result's per-row SubPlan steps and projections over it
    // (Django's annotate(Subquery(..)) on a claim sorted off its index), redone in the recheck too (`sort_tail`)
    if !(p.ops[..j].iter().all(super::lower::sort_emit) || matches!(at, super::lower::Beside::Alone) && super::lower::sort_tail(p, j)) || d.family().is_some() || !alone {
        return None;
    }
    // D3561 REFANTI (procrastinate's fetch_job: NOT EXISTS over earlier jobs of the same lock): or two marks, the other a
    // REFERENCE one whose relation's own scan is the inner of one ANTI join in the producer (rerun/epq/ref_anti.rs), its
    // build's or spool's pipeline the one slot the producer reads, counted beside it (`anti`)
    let (m, r) = match (d.marks.first(), d.marks.get(1)) {
        (Some(m), None) => (m, None),
        _ => crate::rerun::epq::ref_anti::marks(d).map(|(m, r)| (m, Some(r)))?,
    };
    if m.lock == LockKind::None || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel {
        return None;
    }
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let pq = &ps[q];
    let own = match &pq.source {
        Source::Range { am: gravity_decompose::Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: gravity_decompose::Am::Heap, scanrelid, relid, .. } | Source::Index { am: gravity_decompose::Am::Heap, scanrelid, relid, only: false, .. } => u32::from(*scanrelid) == m.rti && *relid == m.relid,
        _ => false,
    };
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_))).unwrap_or(pq.ops.len());
    // D3506 SETLOCK: a CTE claim's producer may probe the sets of locks in hashed SubPlans' plans
    // (rerun/epq/set_lock.rs): the slots it reads besides, the recheck's redo probing sets of its own (`breaker_lock`)
    let sets = if matches!(at, super::lower::Beside::Cte) { crate::rerun::epq::set_lock::set_slots(gx) } else { PgVec::new_in(gx.mcx) };
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || crate::rerun::epq::set_lock::set_probe(o, &sets);
    let ja = r.and_then(|_| pq.ops[..jq].iter().position(|o| !plain(o)));
    let anti = match (r, ja) {
        (None, _) => None,
        (Some(r), Some(ja)) => Some(crate::rerun::epq::ref_anti::inner(gx, q, ja, r)?),
        (Some(_), None) => return None,
    };
    let below = pq.ops[..jq].iter().enumerate().all(|(i, o)| plain(o) || Some(i) == ja) && matches!(pq.ops[..jq].last(), Some(Op::Project(_)));
    let after = pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_)));
    let extra = anti.flatten();
    let nsets = pq.desc.consumes.iter().filter(|s| sets.contains(s)).count();
    let reads = pq.desc.consumes.len() == usize::from(extra.is_some()) + nsets && (!matches!(at, super::lower::Beside::Alone) || Census(gx).alone_with(k, j, q, extra) || extra.is_none() && Census(gx).beside_drained(k, q));
    // under a LIMIT / OFFSET; D3562 REFANTI: or with no bound over that anti join (the runner's recheck opens no anti join)
    let bound = p.ops[j + 1..].iter().any(super::lower::limit_op) || anti.is_some();
    (own && below && after && reads && bound).then_some((q, jq, m.ctid_col))
}

/// D3565 REFDRAIN (`ref_drain`): the lock pipeline's Sort producer `q`, a Hash Right Anti Join's drain (its Drain op `jd`,
/// its sort build `jq`), the join's build `build` (pipeline, its build op) and the locked mark's junk ctid column `col`.
pub(crate) struct RefDrain {
    pub(crate) q: usize,
    pub(crate) jd: usize,
    pub(crate) jq: usize,
    pub(crate) build: (usize, usize),
    pub(crate) col: i16,
}

/// D3565 REFDRAIN (REFBUILD's drain mirror): a lock over a Sort whose producer is a Hash Right Anti Join's drain, the locked
/// relation the join's build (`SELECT t.id FROM t WHERE .. AND NOT EXISTS (SELECT 1 FROM l WHERE l.t_id = t.id) ORDER BY
/// .. FOR UPDATE`, PostgreSQL's LockRows -> Sort -> Hash Right Anti Join: Seq Scan l, Hash <- the scan of t). Two marks,
/// the locked relation's and a REFERENCE one; the build its own heap scan under filters, gates and projections; the probe
/// the REFERENCE relation's own heap scan under the same; the drain's ops plain up to the sort build; only projections and
/// a LIMIT / OFFSET above the lock; no InitPlan, family or other pipeline. A drained row's REFERENCE junk ctid is NULL, so
/// PostgreSQL's EvalPlanQual finds no test tuple for that relation (EvalPlanQualFetchRowMark), its scan is empty and the
/// join drains the newest version of the locked row when it passes the build's quals: the IR recheck redoes the build's
/// part over it, the probe side's columns NULL, then the drain's ops up to the Sort (epq_ir.rs `epq_of_drain`). The NOT
/// EXISTS is not tested again, as PostgreSQL does not test it.
fn ref_drain(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<RefDrain> {
    use crate::rerun::epq::ref_nest::{heap, part_reads_subplan};
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    if !gx.graph.init_plans.is_empty() || p.desc.produces().is_some() || p.desc.consumes.len() != 1 || !Census(gx).whole(4) {
        return None;
    }
    if !p.ops[..j].iter().all(super::lower::sort_emit) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || super::lower::limit_op(o)) {
        return None;
    }
    let (m, r) = crate::rerun::epq::ref_anti::marks(d)?;
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let pq = &ps[q];
    let Source::Breaker { slot: hs } = pq.source else { return None };
    let Some(Op::Join(JoinDesc::Drain { matches, jointype: types_nodes::jointype::JoinType::JOIN_RIGHT_ANTI })) = pq.ops.first() else { return None };
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    if !pq.ops[1..jq].iter().all(plain) || !matches!(pq.ops[..jq].last(), Some(Op::Project(_))) || !pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) || pq.desc.consumes.len() != 2 || part_reads_subplan(pq, jq) {
        return None;
    }
    let b = (0..q).rev().find(|&x| ps[x].desc.produces() == Some(hs))?;
    let pb = &ps[b];
    let jb = pb.ops.len().checked_sub(1)?;
    if !matches!(pb.ops[jb], Op::Join(JoinDesc::Build { .. })) || !pb.ops[..jb].iter().all(plain) || !pb.desc.consumes.is_empty() || heap(&pb.source) != Some((m.rti, m.relid)) || part_reads_subplan(pb, jb) {
        return None;
    }
    let pr = (0..q).rev().find(|&x| ps[x].desc.produces() == Some(*matches))?;
    let pp = &ps[pr];
    let jp = pp.ops.iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Probe { slot: ph, jointype: types_nodes::jointype::JoinType::JOIN_RIGHT_ANTI, .. }) = &pp.ops[jp] else { return None };
    if *ph != hs || heap(&pp.source) != Some((r.rti, r.relid)) || pp.desc.consumes.len() != 1 {
        return None;
    }
    Some(RefDrain { q, jd: 0, jq, build: (b, jb), col: m.ctid_col })
}

/// D3502 FAMCTE (index order: the claim CTE as `Limit -> LockRows -> Index Scan` on the one leaf its queue's name prunes
/// to, over an index in claim order; or no LIMIT, the CTE locking every row it reads): the lock's site test `write_pair` asks when the lock pipeline is a CTE's
/// store: `one_site`, or a family's lock over its own leaf's scan alone (`own_leaf`). The parent's mark is inactive and
/// the one leaf's scan the lock's whole input (`write_pair` refuses a second Lock op: the arms of several leaves), so the
/// product form's IR recheck over that scan is PostgreSQL's EvalPlanQual over the leaf's test tuple. Inside a CTE only
/// `write_pair`'s `in_cte` form admits it (the store read whole by the statement's write).
fn cte_site(p: &gravity_decompose::PipelineNode<'_>, j: usize, d: &LockDesc<'_>) -> bool {
    super::lower::one_site(p, j, d) || super::lower::own_leaf(p, j, d)
}

#[cfg(test)]
mod tests {
    use super::{unordered, Edge, FormId, REFINES};

    #[test]
    fn refines_rows_follow_the_chain() {
        for &(a, b, _, why) in REFINES {
            assert!((a as u32) < (b as u32) && !why.is_empty(), "REFINES {a:?} -> {b:?}");
        }
    }

    #[test]
    fn overlap_names_each_unordered_pair() {
        let one = FormId::Sort15b.bit();
        assert_eq!(unordered(one, FormId::Sort15b, &[]).count(), 0);
        assert_eq!(unordered(0, FormId::Runner, &[]).count(), 0);
        // D4157 INITSORT's B: a 15b lock answered by the runner, with no REFINES row, is the overlap Sort15b/Runner
        assert_eq!(unordered(one, FormId::Runner, &[]).collect::<Vec<_>>(), [(FormId::Sort15b, FormId::Runner)]);
        assert_eq!(unordered(one, FormId::Runner, &[(FormId::Sort15b, FormId::Runner, Edge::Declines, "declined")]).count(), 0);
        // a 15b lock the marks form answers while 15b's test accepts it: 15b has no admission to decline it
        let marks = one | FormId::Marks.bit();
        assert_eq!(unordered(marks, FormId::Marks, &[(FormId::Sort15b, FormId::Marks, Edge::Wins, "LIMIT_ABOVE")]).collect::<Vec<_>>(), [(FormId::Sort15b, FormId::Marks)]);
        assert_eq!(unordered(marks, FormId::Sort15b, &[(FormId::Sort15b, FormId::Marks, Edge::Wins, "LIMIT_ABOVE")]).count(), 0);
        let three = one | FormId::Marks.bit();
        assert_eq!(unordered(three, FormId::Runner, &[(FormId::Sort15b, FormId::Runner, Edge::Declines, "x")]).collect::<Vec<_>>(), [(FormId::Sort15b, FormId::Marks), (FormId::Marks, FormId::Runner)]);
    }
}
