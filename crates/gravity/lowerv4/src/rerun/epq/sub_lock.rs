//! lockrows SUBLOCK (D1018 sub-shape E: lk-0060 / lk-0068; main 2026-10-04T20:29:48Z): a LockRows inside a nested SubPlan's
//! embedded inner, `UPDATE job j .. WHERE EXISTS (SELECT 1 FROM job k WHERE k.id = j.id .. FOR UPDATE [SKIP LOCKED])`.
//! PostgreSQL never pulls a sublink holding row marks up into a join (simplify_EXISTS_query), so the SubPlan keeps its own
//! LockRows over the inner relation's scan. ExecSubPlan rescans it per outer row and pulls rows only until the sublink's
//! answer is known (EXISTS: the first); the LockRows locks each row it hands on (a SKIP LOCKED, deleted or self-modified
//! one is skipped: the scan's next row) and, over a newer version, runs its own EvalPlanQual: the scan alone over the test
//! tuple (its index quals, its filters and projection) under the outer row's parameters, its row going on to the sublink's
//! test. A lock is no raise-free node (decompose rerun.rs), so the subplan home pulls no row past PostgreSQL's.
//! Served (`form`): the inner is that lock over its one locking mark's own heap scan (range / index / bitmap), only filters,
//! gates and projections below it, the last of them the scan's projection carrying the mark's junk ctid, and only
//! projections above it (or a LIMIT: SUBLOCK-LIMIT below); the SubPlan op is the statement's one graph pipeline's (beside the InitPlans' own, AGA-35 form 6:
//! none a CTE's, none holding it, none attached inside another inner, `inner_inits`); the statement's one LockRows; its one write, if any, that pipeline's own UPDATE / DELETE, each InitPlan beside it read before any row
//! reaches it (lock_init.rs `admit_sub`, INIT_READ). The lock's position is the row's junk ctid (the
//! inner's walk sets no locator) and its recheck the IR's (epq_ir.rs over the inner's source and ops below the lock); beside
//! the write it is a `Program.locks` entry of its own (lock_init.rs JOBQ's form: its own lock host), the write keeping the
//! program's plan. The write's EvalPlanQual runs the SubPlan again over the newest target row, and PostgreSQL's plan tree
//! runs the SubPlan's LockRows there too: it locks again (our own lock is TM_Ok; over a newer version of the inner row it
//! rechecks that version), so a changed parameter or a concurrently changed inner row answers as PG's. The write rechecks
//! in the IR for that (modifytable's rowop_epq.rs D652-S1 `subs_in_ir` takes such a pipeline, `subs_lock`): its redo
//! evaluates the SubPlan step again, whose lock locks and rechecks as in the first pass. The runner's recheck node never
//! locks again (driver entry.rs D604: every lock host in EPQ mode), so a runner-rechecked write or lock over such an inner
//! refuses by name (SUBLOCK_RUNNER, `runner_check`, rerun/epq/marks.rs `sites_in`). D3379 SUBLOCK-LEADER: a pipeline
//! decompose leaves parallel-eligible runs on the leader, the lock host's (ops/lockrows/lower.rs `sub_lock`). Any other
//! form keeps D1018.
//! D3667: beside data-modifying CTEs only the outer query's InitPlans read, each writing by trigger-free INSERT (D3379
//! INITCTE, rerun/epq/inits.rs `init_ctes`; `WITH x AS (INSERT .. RETURNING id) SELECT .. WHERE j.id <= (SELECT min(id)
//! FROM x) AND EXISTS (.. FOR UPDATE)`), the statement writing nothing else: PostgreSQL runs the CTE when the InitPlan first
//! reads it and the inner LockRows' EvalPlanQual reruns its scan alone; the INSERT keeps the program's plan and the lock
//! is a `Program.locks` entry of its own (ops/lockrows/lower.rs `sub_lock`).
//! D3274: the subquery's own InitPlans, attached inside the inner (at its LockRows or below), are served beside the lock.
//! ExecInitSubPlan arms each in the statement's estate and the inner's first read runs it. The inner LockRows' own
//! EvalPlanQual forces each pending InitPlan its subplan's extParam holds before the recheck (EvalPlanQualStart /
//! EvalPlanQualBegin, ExecSetParamPlanMulti: one that raises raises there), which the IR recheck does at its start
//! (`forced`). Its fresh estate also re-arms every InitPlan inside a SubPlan's plan, so the recheck runs such an
//! InitPlan again where it reads it, once per EvalPlanQual state: the inner lock's (from its first recheck to its
//! input's end, EvalPlanQualEnd), the ModifyTable's (the statement), and the inner lock's inside the ModifyTable's
//! recheck (that estate's own LockRows). The IR recheck reads the statement's value where a second run shows nothing
//! (`fresh_ok`), and arms any other one in its state (`armed`, `write_recheck`: exec initplan raw.rs `keyed`), the inner
//! lock's state ending at its inner's end (ops/lockrows/lower.rs `Lock::ended`).
//! D3379 SUBLOCK-LIMIT (lockrows_sublock :88, `.. WHERE EXISTS (SELECT 1 FROM job k WHERE k.queue = j.queue AND k.id >
//! j.id ORDER BY k.id LIMIT 1 FOR UPDATE)`): simplify_EXISTS_query keeps the LIMIT of a sublink holding row marks, so
//! the SubPlan is Limit -> LockRows -> scan. ExecLimit counts only the rows LockRows hands on (each locked, and over a
//! newer version rechecked: a version that fails is skipped and the scan's next row locked), so the inner's LIMIT step
//! above the lock (rerun/inner_limit.rs, D1018 row A) ends the re-open's walk where PG's Limit stops pulling, and no row
//! past it is locked. The offset's rows are locked as PG's Limit pulls and drops them. Served with the lock's other ops
//! as above. Not WITH TIES (it reads past the count, locking the row it peeks at). Not with an InitPlan attached inside
//! the inner: its LockRows never sees its input's end under a met count, so PG keeps that lock's EvalPlanQual state
//! (EvalPlanQualEnd never runs) where `armed`'s state ends at the walk's end.
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::{Am, InitPlan, Op, PipelineNode, Source};
use gravity_program::program::LockSite;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_program::{Cmd, LockKind};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::ops::lockrows::plan::Census;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::end::EndMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::ops::lockrows::lock_init;

/// The runner's recheck of a write or lock whose pipeline embeds an inner that locks: its node never locks (D604).
const SUBLOCK_RUNNER: ShapeWhy = ShapeWhy("a runner-rechecked write or lock over an inner that locks rows (the recheck node never locks again, D604; lockrows SUBLOCK)");

/// `form`'s answer: the lock's junk ctid column, its locking mark as a `Program.locks` entry, the graph pipeline whose
/// SubPlan op embeds the inner, and whether the statement writes (the lock then beside the write, `LockSite` mark 1).
/// D3506 EXISTS: `claim`, the locking mark of a claim of the outer pipeline the lock is beside instead (`claim_at`):
/// both are `Program.locks` entries, the claim's mark 1 and the lock's 2 (the driver lays either the program's one lock
/// or its `Program.locks`).
pub(crate) struct SubLock {
    pub ctid: i16,
    pub site: LockSite,
    pub outer: usize,
    pub write: bool,
    pub claim: Option<LockSite>,
    /// D3660: beside the InitPlan whose own row lock is the M4 pair's A (`form`'s `pair`): this lock is B, `Program.locks`'
    /// entry 2 (plan.rs `Plan::role` lays both), A's 1.
    pub init: bool,
    /// D3667: beside data-modifying CTEs that only the statement's non-CTE InitPlans read, each writing by trigger-free
    /// INSERT (rerun/epq/inits.rs `init_ctes`, D3379 INITCTE), the statement writing nothing else: the INSERT keeps the
    /// program's plan and this lock is `Program.locks`' entry 1.
    pub cte: bool,
}

/// rerun/inner.rs `make`'s op_ok (no Memoize): op `i` of inner `ik`, which op `j` of pipeline `k` embeds, is a lock this
/// file serves (`form`), so the inner is made.
pub(crate) fn inner_ok(gx: &Gx<'_, '_>, (k, j): (usize, usize), ik: usize, i: usize) -> bool {
    matches!(gx.pipelines()[k].ops.get(j), Some(Op::SubPlan(_))) && form(gx, ik, i).is_some()
}

/// The module doc's served form for the lock at op `j` of pipeline `ik`, else none.
pub(crate) fn form(gx: &Gx<'_, '_>, ik: usize, j: usize) -> Option<SubLock> {
    let ps = gx.pipelines();
    let n = ps.len();
    let inners = ps.inners();
    let &((k, jo), ip) = inners.get(ik.checked_sub(n)?)?;
    let Some(Op::Lock(d)) = ip.ops.get(j) else { return None };
    // D3506 EXISTS: or one lock more, the claim above the SubPlan op (`claim_at`); EXISTS SORT: its pipeline one more
    let at = if k < n { claim_at(gx, k, jo) } else { None };
    let sorted = at.as_ref().is_some_and(|x| x.0 .0 != k);
    if k >= n || !matches!(ps[k].ops.get(jo), Some(Op::SubPlan(_))) || n != 1 + usize::from(sorted) + inner_inits(gx, k, ip)? {
        return None;
    }
    let locks = ps.iter().chain(inners.iter().map(|x| x.1)).flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
    let claim = at.map(|x| x.1);
    // D3660: or, with no claim and no write, one lock more in a non-CTE InitPlan's pipelines (the M4 pair's A: its run
    // rechecks under its own EvalPlanQual, this lock's recheck copies the InitPlan's value)
    let pair = claim.is_none() && ps.iter().enumerate().any(|(q, p)| Census(gx).init_member(q) && p.ops.iter().any(|o| matches!(o, Op::Lock(_))));
    let (Some(m), None) = (d.marks.first(), d.marks.get(1)) else { return None };
    if locks != 1 + usize::from(claim.is_some()) + usize::from(pair) || d.family().is_some() || m.lock == LockKind::None || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    // D3379 SUBLOCK-LIMIT (`limit`): or a LIMIT / OFFSET above the lock, not WITH TIES, when no InitPlan is attached inside
    // the inner
    let inner_init = gx.graph.init_plans.iter().any(|i| ip.node_ops.iter().any(|x| x.node_id == i.attach));
    let above = |o: &Op<'_>| matches!(o, Op::Project(_)) || !inner_init && crate::frame::iface::demand(o).is_some_and(|l| !l.ties);
    if !own_scan(ip, m.rti, m.relid) || !ip.ops[..j].iter().all(plain) || !ip.ops[j + 1..].iter().all(above) {
        return None;
    }
    // the scan's projection, last below the lock, carries the mark's junk ctid: the scanned relation's ctid
    let Some(Op::Project(l)) = ip.ops[..j].last() else { return None };
    let ctid = l.iter().filter_map(|x| x.as_target_entry()).any(|te| te.resno == m.ctid_col && te.expr.as_var().is_some_and(|v| v.varattno == -1 && v.varlevelsup == 0 && i64::from(v.varno) == i64::from(m.rti)));
    // the statement's one write, if any: that pipeline's own UPDATE / DELETE, outside every CTE. D3667: or only INITCTE's
    // INSERTs (`inner_inits` admitted their CTEs): PostgreSQL runs such a CTE when the InitPlan first reads it, before the
    // outer scan's first row reaches the SubPlan, and the inner lock's EvalPlanQual reruns only the inner's scan (no CTE,
    // no InitPlan of the outer query)
    let ctes = crate::rerun::epq::inits::init_ctes(gx)?;
    let cte_ins = ps.iter().enumerate().any(|(q, p)| ctes.contains(&q) && p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd != Cmd::Select)));
    let mut writes = ps.iter().enumerate().filter(|&(q, _)| !ctes.contains(&q)).flat_map(|(_, p)| p.ops.iter()).filter_map(|o| match o {
        Op::Write(w) if w.cmd != Cmd::Select => Some(w.cmd),
        _ => None,
    });
    let write = match (writes.next(), writes.next()) {
        (None, _) if cte_ins && (claim.is_some() || pair) => return None,
        (None, _) => false,
        (Some(Cmd::Update | Cmd::Delete), None) if !gx.cte_write(k) && claim.is_none() && !pair && !cte_ins => true,
        _ => return None,
    };
    let site = LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait };
    ctid.then_some(SubLock { ctid: m.ctid_col, site, outer: k, write, claim, init: pair, cte: cte_ins })
}

/// D3506 EXISTS (graphile-worker <= 0.13's get_job: `SELECT .. FROM jobs WHERE .. AND (jobs.queue_name IS NULL OR
/// EXISTS (SELECT 1 FROM job_queues WHERE job_queues.queue_name = jobs.queue_name .. FOR UPDATE SKIP LOCKED)) .. ORDER
/// BY .. LIMIT 1 FOR UPDATE SKIP LOCKED`): graph pipeline `k`, whose op `jo` embeds the inner, holds one LockRows above
/// that op, under a LIMIT / OFFSET (D3707: or none), over its own relation alone with only filters, gates, projections
/// and SubPlan steps below it. PostgreSQL's claim LockRows rechecks a newer version under its own EvalPlanQual: the scan's quals again
/// over the test tuple, the EXISTS SubPlan run again with the new row's parameters in its fresh estate, whose LockRows
/// locks again (our own lock TM_Ok, a row another session holds skipped). The claim rechecks in the IR with its SubPlan
/// steps redone (ops/lockrows/lower.rs `subs_in_ir`); it and the inner's lock are `Program.locks` entries, marks 1 and
/// 2 (`SubLock::claim`, `sub_claim`). D3506 EXISTS SORT (over a few jobs: Limit -> LockRows -> Sort -> Seq Scan, the
/// EXISTS in the scan's filter): or pipeline `k` is the Sort's producer, its relation's own heap scan with only
/// filters, gates, projections (the last its row) and nested SubPlan steps before its sort build, and the claim is the
/// lock of the one pipeline reading that Sort, over the Sort emit's projection alone (ops/lockrows/plan.rs `breaker_limit`'s 15b
/// form). PG's EvalPlanQual reruns Sort -> scan over the test tuple, the EXISTS locking again; the claim's recheck
/// redoes the producer's part, its SubPlan steps made again for it (`redo_subs`, `SortClaim`). The claim's pipeline and
/// op, and its locking mark, else none.
fn claim_at(gx: &Gx<'_, '_>, k: usize, jo: usize) -> Option<((usize, usize), LockSite)> {
    let ps = gx.pipelines();
    let p = ps.get(k)?;
    let below = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_));
    let (c, jl, d, sorted) = match lock_of(p) {
        Some((jl, d)) => (k, jl, d, false),
        None => {
            let jq = p.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
            let slot = p.desc.produces()?;
            let mut readers = ps.iter().enumerate().filter(|(_, x)| matches!(x.source, Source::Breaker { slot: s } if s == slot));
            let (Some((c, pc)), None) = (readers.next(), readers.next()) else { return None };
            let (jl, d) = lock_of(pc)?;
            // nested SubPlan steps only (each made again for the recheck, `redo_subs`), the producer's row last
            let nested = |(i, o): (usize, &Op<'_>)| !matches!(o, Op::SubPlan(_)) || ps.inner_of(k, i).is_some();
            let part = jo < jq && p.ops[..jq].iter().all(below) && p.ops[..jq].iter().enumerate().all(nested) && matches!(p.ops[..jq].last(), Some(Op::Project(_)));
            if c <= k || !part || !p.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) || !pc.ops[..jl].iter().all(crate::ops::lockrows::lower::sort_emit) {
                return None;
            }
            (c, jl, d, true)
        }
    };
    let pc = &ps[c];
    let limit = pc.ops[jl + 1..].iter().any(|o| crate::frame::iface::limit_bounds(o).is_some());
    let (Some(m), None) = (d.marks.first(), d.marks.get(1)) else { return None };
    let own = d.family().is_none() && m.lock != LockKind::None && !m.parent;
    let other = ps.iter().enumerate().any(|(q, x)| q != c && x.ops.iter().any(|o| matches!(o, Op::Lock(_))));
    // EXISTS SORT: the Sort carries the mark's junk ctid of the producer's own scan
    let fits = match sorted {
        true => m.ctid_col > 0 && m.relid == d.plan.target_rel && own_scan(p, m.rti, m.relid),
        false => jl > jo && p.ops[..jl].iter().all(below),
    };
    // D3707 (lockrows_sublock :96, `.. WHERE j.id <= 12 AND EXISTS (.. FOR SHARE) FOR UPDATE OF j`): or, over the scan
    // itself, no LIMIT: ExecLockRows rechecks a newer version under the same EvalPlanQual with or without one
    ((limit || !sorted) && own && !other && fits).then_some(((c, jl), LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait }))
}

/// Pipeline `p`'s one LockRows: its op and description, else none.
fn lock_of<'a, 'p>(p: &'a PipelineNode<'p>) -> Option<(usize, &'a gravity_decompose::ops::shared::modifytable::LockDesc<'p>)> {
    let mut at = p.ops.iter().enumerate().filter_map(|(i, o)| if let Op::Lock(d) = o { Some((i, d)) } else { None });
    match (at.next(), at.next()) {
        (Some(x), None) => Some(x),
        _ => None,
    }
}

/// D3506 EXISTS: the lock at op `j` of graph pipeline `k` is the claim beside an inner lock this file serves
/// (`claim_at`): `Program.locks`' entry 1.
pub(crate) fn sub_claim(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    let n = ps.len();
    k < n && matches!(ps[k].ops.get(j), Some(Op::Lock(_))) && ps.inners().iter().enumerate().any(|(x, ((q, jo), ip))| *q < n && claim_at(gx, *q, *jo).is_some_and(|c| c.0 == (k, j)) && ip.ops.iter().enumerate().any(|(i, o)| matches!(o, Op::Lock(_)) && form(gx, n + x, i).is_some_and(|s| s.claim.is_some())))
}

/// D3506 EXISTS SORT (`claim_at`): the claim at op `j` of graph pipeline `k` reads the Sort of the pipeline whose
/// SubPlan step embeds the inner lock: that producer, its sort build's op and the mark's junk ctid column, else none.
pub(crate) fn sort_claim(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(usize, usize, i16)> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps.get(k)?.source else { return None };
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let jq = ps[q].ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    let (_, d) = lock_of(&ps[k]).filter(|x| x.0 == j)?;
    let m = d.marks.first()?;
    (ps.inners().iter().any(|((x, _), _)| *x == q) && sub_claim(gx, k, j)).then_some((q, jq, m.ctid_col))
}

/// D3506 EXISTS SORT: the SubPlan steps among the Sort producer `q`'s ops before its sort build `jq`, each made again
/// for the claim's recheck (subplan per_row.rs `redo_sub`: its inner and the inner's lock made again), by SubPlan id;
/// the claim lends them to its redo while it lowers (`SortClaim`): the producer's own steps lower in its function.
pub(crate) fn redo_subs<'g>(px: &mut Px<'g, '_>, q: usize, jq: usize) -> PgVec<'g, (i32, PgBox<'g, dyn StepMech + 'g>)> {
    let (gx, mcx) = (px.gx, px.mcx());
    (0..jq).filter(|&i| matches!(gx.pipelines()[q].ops[i], Op::SubPlan(_))).map(|i| crate::ops::subplan::per_row::redo_sub(px, q, i)).collect_in(mcx)
}

/// D3506 EXISTS SORT: the claim's lock (`lock`) with the SubPlan steps of its Sort's producer made again for its
/// recheck (`own`, `redo_subs`): each one's prologue, epilogue and declarations run with the lock's, and while the lock
/// lowers each is lent to its recheck's redo by SubPlan id (`Env::redo_own`, epq_ir.rs `Redo::Sub`), which lowers it
/// over the redo's row as the producer's step was over the scan's.
pub(crate) struct SortClaim<'g> {
    pub lock: PgBox<'g, dyn StepMech + 'g>,
    pub own: PgVec<'g, (i32, PgBox<'g, dyn StepMech + 'g>)>,
}

impl StepMech for SortClaim<'_> {
    fn end_is(&mut self, end: &dyn EndMech) {
        self.lock.end_is(end);
    }

    fn open_words(&self, w: &mut OpenWords) {
        self.lock.open_words(w);
        self.own.iter().for_each(|s| s.1.open_words(w));
    }

    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.lock.prologue(b, env, x);
        self.own.iter().for_each(|s| s.1.prologue(b, env, x));
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let refs: PgVec<'_, &dyn StepMech> = self.own.iter().map(|s| &*s.1 as &dyn StepMech).collect_in(env.mcx);
        let n = env.redo_own.len();
        env.redo_own.extend_in(self.own.iter().zip(refs.iter()).map(|(s, r)| (s.0, r as *const &dyn StepMech as usize)));
        let c = self.lock.lower(b, env, row, next);
        env.redo_own.truncate(n);
        c
    }

    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.lock.epilogue(b, env, x);
        self.own.iter().for_each(|s| s.1.epilogue(b, env, x));
    }

    fn reentry(&self) -> Option<i32> {
        self.lock.reentry()
    }

    fn ends(&self) -> bool {
        self.lock.ends()
    }

    fn ended(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.lock.ended(b, env);
    }

    fn wants_locator(&self) -> bool {
        self.lock.wants_locator()
    }

    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.lock.declare(pb);
        self.own.iter_mut().for_each(|s| s.1.declare(pb));
    }

    fn serial(&self) -> bool {
        self.lock.serial() || self.own.iter().any(|s| s.1.serial())
    }
}

/// D3506 EXISTS SORT: SubPlan `id`'s step a claim's lock lends its recheck's redo (`SortClaim`), else none.
pub(crate) fn redo_own<'a>(env: &Env, id: i32) -> Option<&'a dyn StepMech> {
    let &(_, at) = env.redo_own.iter().rev().find(|s| s.0 == id)?;
    Some(crate::ops::subplan::raw::step_at(at))
}

/// D3274 (`form`): plan.rs `Census::sub_inits` with the subquery's own InitPlans: one attached inside the lock's inner `ip`
/// (at its LockRows or below) is served when its pipelines lock and write nothing and none is a rerun set's
/// (`plain_init`). The number of pipelines the statement's InitPlans run when none is a CTE's (D3667: or each CTE's is
/// INITCTE's, `init_ctes`), none holds pipeline `k` and none is attached inside another inner, else none (D1018 keeps those).
fn inner_inits(gx: &Gx<'_, '_>, k: usize, ip: &PipelineNode<'_>) -> Option<usize> {
    let inners = gx.pipelines().inners();
    let at = |p: &PipelineNode<'_>, i: &InitPlan<'_>| p.node_ops.iter().any(|n| n.node_id == i.attach);
    // D3667: a CTE's InitPlan when the CTE is INITCTE's (rerun/epq/inits.rs `init_ctes`: only the non-CTE InitPlans read
    // it, it writes only by trigger-free INSERT, no row lock in it); its pipelines are among theirs (`init_pipelines`)
    let ctes = crate::rerun::epq::inits::init_ctes(gx)?;
    let ok = |i: &InitPlan<'_>| match i.link {
        SubLink::Cte => ctes.contains(&usize::from(i.pipeline)),
        _ => at(ip, i) && lock_init::plain_init(gx, i) || !inners.iter().any(|x| at(x.1, i)),
    };
    if !gx.graph.init_plans.iter().all(ok) {
        return None;
    }
    let init = Census(gx).init_pipelines();
    (!init.contains(&k)).then_some(init.len())
}

/// D3274: whether a second run of InitPlan `i` shows nothing, so the IR recheck may read the statement's value where the
/// inner lock's fresh recheck estate runs it again (the effects home's `init_rerun_alike` over the pipelines it runs).
fn fresh_ok(gx: &Gx<'_, '_>, i: &InitPlan<'_>) -> bool {
    crate::effects::visible::subplan::init_rerun_alike(gx, i, &gx.with_producers([usize::from(i.pipeline)]))
}

/// D3274 (ops/lockrows/epq.rs `epq_of_sub`): the InitPlans the IR recheck of the lock at op `j` of inner `ik` forces at its
/// start, as that LockRows' EvalPlanQualStart / EvalPlanQualBegin force each pending one its subplan's extParam holds
/// (ExecSetParamPlanMulti, by parameter): every non-CTE InitPlan the inner's source keys, recheck quals and ops below the
/// lock may read (lock_init.rs `write_may_read`, whose part is the same over these ops). Each one's first parameter and
/// handle word (rerun/params.rs `init_handle`).
pub(crate) fn forced<'g>(px: &mut Px<'g, '_>, ik: usize, j: usize) -> PgVec<'g, (i32, u16)> {
    let gx = px.gx;
    let mcx = px.mcx();
    let Some(p) = gx.pipelines().get(ik) else { return PgVec::new_in(mcx) };
    let mut ids: PgVec<'_, i32> = gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte) && lock_init::write_may_read(p, j, i.params)).map(|i| i.param).collect_in(px.mcx());
    ids.sort_unstable();
    ids.into_iter().map(|id| (id, crate::rerun::params::init_handle(px, id))).collect_in(mcx)
}

/// D3274 (ops/lockrows/epq.rs `epq_of_sub`): the InitPlans the IR recheck of the lock in inner `ik` arms in its own
/// EvalPlanQual state (the lock's plan node + 1: `Epq::keyed`), as that LockRows' fresh recheck estate re-arms every
/// InitPlan inside a SubPlan's plan: each one attached inside the inner whose second run shows (`fresh_ok`), by its handle
/// word, first parameter and parameter count (lock_init.rs `armed`'s form).
pub(crate) fn armed<'g>(px: &mut Px<'g, '_>, ik: usize) -> PgVec<'g, (u16, i32, u32)> {
    let gx = px.gx;
    let mcx = px.mcx();
    let Some(p) = gx.pipelines().get(ik) else { return PgVec::new_in(mcx) };
    gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte) && p.node_ops.iter().any(|n| n.node_id == i.attach) && !fresh_ok(gx, i)).map(|i| (crate::rerun::params::init_handle(px, i.param), i.param, i.params.len() as u32)).collect_in(mcx)
}

/// The plan node of pipeline `p`'s write (lock_init.rs `admit_sub`'s): the last node starting at or before the write op,
/// the ModifyTable whose recheck decompose's `recheck_inits` describe when it is the statement's `recheck_node`.
fn write_node(p: &PipelineNode<'_>) -> i32 {
    let jw = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    p.node_ops.iter().rev().find(|n| usize::from(n.first_op) <= jw).map_or(p.node_id, |n| n.node_id)
}

/// A write recheck's InitPlans (`write_recheck`): those it forces at its start (first parameter, handle word), those it arms
/// (handle word, first parameter, parameter count) and its EvalPlanQual state's key.
pub(crate) type WriteInits<'g> = (PgVec<'g, (i32, u16)>, PgVec<'g, (u16, i32, u32)>, u32);

/// D3274 (epq_ir.rs `epq_of_subs`): a write over graph pipeline `k` whose SubPlan step embeds this file's lock (`subs_lock`)
/// rechecks in the IR (the runner's recheck never locks again), as the ModifyTable's EvalPlanQual: at its start it forces
/// the pending InitPlans EvalPlanQualStart / EvalPlanQualBegin force (lock_init.rs `forced` at `write_node`), so a
/// conditional read below the write raises there as PG's does, and in its own state (the write's plan node + 1) it arms
/// those inside the embedded inners that the fresh estate runs again (`armed`). `None` for any other pipeline.
pub(crate) fn write_recheck<'g>(px: &mut Px<'g, '_>, k: usize) -> Option<WriteInits<'g>> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let p = ps.get(k).filter(|_| k < ps.len() && subs_lock(gx, k))?;
    let node = write_node(p);
    let force = lock_init::forced(px, node);
    let n = ps.len();
    let mut arm = PgVec::new_in(px.mcx());
    for ik in ps.inners().iter().enumerate().filter(|(_, ((q, _), _))| *q == k).map(|(x, _)| n + x) {
        arm.extend_in(armed(px, ik));
    }
    Some((force, arm, node as u32 + 1))
}

/// D3274 (ops/modifytable/rowop_epq.rs `subs_in_ir`, D2904's `kept_inits`): rerun/epq/inits.rs `write_inits_read` for a
/// write at op `j` of graph pipeline `k` whose SubPlan step embeds this file's lock, whose IR recheck forces at its start
/// (`write_forced`): each InitPlan of this query level the write's part may read is read there first (lock_init.rs
/// `read_first`, with the write's projections) or is one that recheck forces (lock_init.rs `forces` at `write_node`), so
/// a computed index key or a conditional read over it rechecks as PostgreSQL's.
pub(crate) fn write_inits_forced(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    let Some(p) = ps.get(k).filter(|_| k < ps.len() && subs_lock(gx, k)) else { return false };
    let inners = ps.inners();
    let inner = |i: &InitPlan<'_>| inners.iter().any(|x| x.1.node_ops.iter().any(|n| n.node_id == i.attach));
    let node = write_node(p);
    let mut read = gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte) && !inner(i) && lock_init::write_may_read(p, j, i.params));
    read.all(|i| lock_init::read_first(gx, p, j, i, true) || lock_init::forces(gx, node, i))
}

/// Inner `p`'s source is relation `relid`'s own heap scan at range table index `rti` (range, bitmap or index, not ONLY).
fn own_scan(p: &PipelineNode<'_>, rti: u32, relid: u32) -> bool {
    match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid: r, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid: r, .. } | Source::Index { am: Am::Heap, scanrelid, relid: r, only: false, .. } => u32::from(*scanrelid) == rti && *r == relid,
        _ => false,
    }
}

/// modifytable's rowop_epq.rs `subs_in_ir`: graph pipeline `k` runs a SubPlan step whose inner's lock this file serves, so
/// its write rechecks in the IR (module doc), the redo locking again as PG's EvalPlanQual plan tree does.
pub(crate) fn subs_lock(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let n = ps.len();
    ps.inners().iter().enumerate().any(|(x, ((q, j), ip))| *q == k && matches!(ps[k].ops.get(*j), Some(Op::SubPlan(_))) && ip.ops.iter().enumerate().any(|(i, o)| matches!(o, Op::Lock(_)) && form(gx, n + x, i).is_some()))
}

/// rerun/epq/marks.rs `sites_in`, the runner's recheck of pipeline `k`'s write or lock: refused by name (SUBLOCK_RUNNER) when
/// a SubPlan step's inner holds a lock this file serves (any other inner lock refuses D1018 at its make).
pub(crate) fn runner_check(gx: &Gx<'_, '_>, k: usize, node: i32) {
    if k < gx.pipelines().len() && subs_lock(gx, k) {
        crate::refuse!(node, Reason::Shape { why: SUBLOCK_RUNNER });
    }
}
