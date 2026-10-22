//! D2904: the InitPlans a write's EvalPlanQual recheck in the IR may copy (ops/modifytable/rowop_epq.rs `subs_in_ir`,
//! D652-I `kept_inits`). PostgreSQL's EvalPlanQualStart forces each pending InitPlan attached at the ModifyTable or above
//! that the recheck's plan reads (ExecSetParamPlanMulti over its extParam) before the redo, and copies the values; one of a
//! subquery, attached inside its SubPlan's plan, is re-armed in the recheck's estate instead (D652-I's licence). The IR's
//! redo reads the value this execution computed, so it is PostgreSQL's only where every row reaching the write has forced
//! the InitPlan: lockrows' INIT_READ rule (ops/lockrows/lock_init.rs `read_first`), here over the write's part.
use gravity_decompose::ops::modifytable::trig_facts::cmd_bits;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::{Op, PipelineNode};
use mcx::{CollectIn, PgVec};

use crate::ops::lockrows::plan::Census;
use crate::graph::Gx;
use crate::ops::lockrows::lock_init::{read_first, write_may_read};

/// Whether every InitPlan of this query level that pipeline `p`'s part before the write op `j` may read (a SubPlan step
/// through its test and args only) is read there at an unconditional position (`read_first` with `proj`): a filter, an
/// index scan's keys, a TID scan's quals or a projection below the write (an UPDATE's SET). Else the write keeps the
/// runner's recheck, which forces a pending one (D2112 phase B).
pub(crate) fn write_inits_read(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, j: usize) -> bool {
    let inners = gx.pipelines().inners();
    let inner = |i: &gravity_decompose::InitPlan<'_>| inners.iter().any(|x| x.1.node_ops.iter().any(|n| n.node_id == i.attach));
    gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte) && !inner(i) && write_may_read(p, j, i.params)).all(|i| read_first(gx, p, j, i, true))
}

/// D3379 INITCTE (siblings_aga35_*_initplan's CTE bound: `WITH w AS (INSERT .. RETURNING id) SELECT .. WHERE x >=
/// (SELECT min(id) FROM w) .. LIMIT n FOR UPDATE`): the pipelines of the statement's CTEs (each CTE link's pipeline and
/// the producers it reads, `with_producers`) when each is one only the non-CTE InitPlans read: its pipelines among theirs
/// (lockrows plan.rs `Census::init_pipelines` holds them as producers), its product read by none of the others nor by a nest's or SubPlan's
/// inner, no row lock in them, and every write there an INSERT with no check option, no ON CONFLICT, no MERGE and no
/// partition route (it never stops for EvalPlanQual) into relations that fire no trigger for it (D4673: or such an UPDATE /
/// DELETE rechecking in its own IR, no row lock in the statement and no other write into its relation). PostgreSQL runs such a CTE when the InitPlan first reads it (ExecPostprocessPlan finishes it),
/// and a row lock's EvalPlanQual neither runs it nor the InitPlan again (EvalPlanQualStart copies both params' values),
/// as an IR recheck of the lock reads the Init cell this run filled; the program's own run makes the InitPlan's members
/// `recheck_skip` (frame/program.rs) and its writer is one of `Program.writes` beside the lock's plan (modifytable
/// rowop.rs `lock_beside_drained`). Empty without a CTE; none when a CTE is not one.
pub(crate) fn init_ctes(gx: &Gx<'_, '_>) -> Option<Vec<usize>> {
    crate::stack_check();
    let ps = gx.pipelines();
    let init = Census(gx).init_pipelines();
    // D4673: a lock in a SubPlan's inner too (sub_lock.rs `form` asks this again through `cte_in_ir`)
    let locks = ps.iter().chain(ps.inners().iter().map(|x| x.1)).any(|p| p.ops.iter().any(|o| matches!(o, Op::Lock(_))));
    let mut out: Vec<usize> = Vec::new(); // alloc: Program
    for i in gx.graph.init_plans.iter().filter(|i| matches!(i.link, SubLink::Cte)) {
        let c = i.pipeline as usize;
        let s = ps[c].desc.produces()?;
        let own = gx.with_producers([c]);
        let read_by_init = (0..ps.len()).filter(|&m| !own.contains(&m) && ps[m].desc.consumes.contains(&s)).all(|m| init.contains(&m));
        let inner = ps.inners().iter().any(|x| x.1.desc.consumes.contains(&s));
        // no trigger on its targets (fail closed): PostgreSQL's CteScan pulls the INSERT only as far as the InitPlan reads
        // and ExecPostprocessPlan runs the rest after the statement's lock, where this program runs it whole first
        let quiet = |r: u32, cmd: gravity_program::Cmd| {
            let t = gx.facts.trig_ext(r);
            t.known && (t.before_row | t.after_row | t.instead_row | t.statement) & cmd_bits(cmd) == 0
        };
        let plain = |w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| {
            w.checks.is_empty() && w.on_conflict.is_none() && w.merge.is_none() && w.route.is_none() && quiet(w.plan.target_rel, w.cmd) && w.targets.iter().all(|t| quiet(t.relid, w.cmd))
        };
        // D4673 (`WITH d AS (UPDATE t .. RETURNING id) UPDATE c SET n = (SELECT count(*) FROM d) ..`): or an UPDATE /
        // DELETE that rechecks in its own IR (it never stops for the runner's EvalPlanQual), in a statement that locks no
        // row and whose other writes target another relation: run whole first, where PostgreSQL may finish it after the
        // statement's own write, no row's outcome changes
        let elsewhere = |x: usize, rel: u32| {
            ps.iter().enumerate().filter(|&(y, _)| y != x).flat_map(|(_, p)| p.ops.iter()).any(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select && (w.plan.target_rel == rel || w.route.is_some() || w.targets.iter().any(|t| t.relid == rel))))
        };
        let alone = |x: usize, o: &Op<'_>| match o {
            Op::Lock(_) => false,
            Op::Write(w) if w.cmd == gravity_program::Cmd::Insert => plain(w),
            Op::Write(w) if matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete) => !locks && plain(w) && !elsewhere(x, w.plan.target_rel) && crate::ops::modifytable::rowop_epq::cte_in_ir(gx, x),
            Op::Write(w) => w.cmd == gravity_program::Cmd::Select,
            _ => true,
        };
        if !own.iter().all(|x| init.contains(x)) || !read_by_init || inner || !own.iter().all(|&x| ps[x].ops.iter().all(|o| alone(x, o))) {
            return None;
        }
        out.extend(own); // alloc: Program
    }
    Some(out)
}

/// D3489 CTECLAIM-INIT (`WITH d AS (DELETE FROM job WHERE id = (SELECT id FROM job .. LIMIT 1 FOR UPDATE SKIP LOCKED)
/// RETURNING ..) INSERT INTO done SELECT * FROM d`): the InitPlans the IR recheck of a data-modifying CTE's UPDATE / DELETE
/// (write op `j` of the CTE's pipeline `k`) arms for its redo: each non-CTE InitPlan attached at the write's ModifyTable
/// node whose parameters the write's part reads, at an unconditional position (lock_init.rs `read_first`, the projections
/// too), so every row reaching the write has run it once. PostgreSQL's EvalPlanQualStart initializes every subplan's tree
/// afresh in the recheck's EState, the CTE's ModifyTable among them (ExecInitSubPlan arms the InitPlans attached there),
/// so the write's first recheck runs such an InitPlan again where it reads it (a claim's LockRows locking again: its own
/// lock is TM_Ok, a newer version rechecked), and a later recheck of the statement reads the value EvalPlanQualBegin
/// copies from the parent (exec initplan raw.rs `keyed`, the write's state). Their indexes in the graph's `init_plans`;
/// none for any other pipeline or when it reads none.
pub(crate) fn cte_write_armed<'g>(gx: &Gx<'g, '_>, k: usize, j: usize) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let Some(p) = ps.get(k).filter(|_| k < ps.len() && gx.cte_write(k)) else { return PgVec::new_in(gx.mcx) };
    let node = p.node_ops.iter().rev().find(|n| usize::from(n.first_op) <= j).map_or(p.node_id, |n| n.node_id);
    let armed = |i: &gravity_decompose::InitPlan<'_>| !matches!(i.link, SubLink::Cte) && i.attach == node && write_may_read(p, j, i.params) && read_first(gx, p, j, i, true);
    gx.graph.init_plans.iter().enumerate().filter(|(_, i)| armed(i)).map(|(x, _)| x).collect_in(gx.mcx)
}

/// D3489 (epq_ir.rs `epq_of_subs`): the IR recheck of the CTE write over pipeline `k` arms `cte_write_armed`'s InitPlans
/// in its own EvalPlanQual state (the write's plan node + 1), forcing none (each was read before the row reached the
/// write); `None` when it arms none.
pub(crate) fn cte_write_recheck<'g>(px: &mut crate::frame::px::Px<'g, '_>, k: usize) -> Option<crate::rerun::epq::sub_lock::WriteInits<'g>> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = gx.pipelines().get(k).filter(|_| k < gx.pipelines().len())?;
    let j = p.ops.iter().position(|o| matches!(o, Op::Write(_)))?;
    let node = p.node_ops.iter().rev().find(|n| usize::from(n.first_op) <= j).map_or(p.node_id, |n| n.node_id);
    let armed = cte_write_armed(gx, k, j);
    if armed.is_empty() {
        return None;
    }
    let arm = armed
        .into_iter()
        .map(|x| {
            let i = &gx.graph.init_plans[x];
            (crate::rerun::params::init_handle(px, i.param), i.param, i.params.len() as u32)
        })
        .collect_in(mcx);
    Some((PgVec::new_in(mcx), arm, node as u32 + 1))
}
