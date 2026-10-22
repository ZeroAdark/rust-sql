//! The row-op end (plan §3.1 end face; H27, H28; R12-L04, R12-L17; KEEP the ladder's probe write, lower/src/write.rs
//! `write_row` / `after_write` at lowerv2-final). Per row: the source row's locator (H28, `Env::loc`, which the frame sets
//! after the source's `row` when the end is a row op) is stored into the write site's position, then ONE host call,
//! `write_row(ctx, site, op)` (kernels host_abi v4 m1.rs), which is `Program.write` into the `hosts::write::WriteHost` the
//! driver derives on the leader from `Descs::write`. No slot, no projection, no junk filter, no refetch: a DELETE's write
//! site carries no cells, only the position.
//!
//! The host's status, mapped from `TM_Result` as nodeModifyTable's ExecDelete does (hosts/src/write.rs `outcome`):
//! - `WRITE_DONE`: the row went; the driver counts it for the command tag (`add_rows`), not the frame.
//! - `WRITE_SKIPPED`: deleted concurrently, or already modified by this command (TM_SelfModified with cmax = cid): no count.
//! - `WRITE_RECHECK`: updated concurrently under READ COMMITTED; the host locked the newest version (`recheck_pos`). The call
//!   stops with `recheck(source site)` and the cursor after the row. The runner runs EvalPlanQual as a second node with that
//!   site substituted (exec/src/epq.rs; the index source re-applies its keys as quals, PG's IndexRecheck, and the filters
//!   run again), then resumes this call past the row (H17 b: the resume point is the window cursor the stop writes back).
//! - below zero: the host's error (a serialization failure, 27000 ...), leaving with the rows finished before it (ENTRY §E3).
//!
//! Width (R12-L17, H15 b): a row op runs leader-only with no pool wake. Decompose makes a write's pipeline serial
//! (`Reason::Write`), road `width.write` (frame/width.rs) declares it to the driver, and this end refuses a pipeline that is
//! parallel-eligible (a Bug: never a write on a worker). The carry's `out` counts the rows that reached the end (the scan
//! node's EXPLAIN ANALYZE rows), whatever their status; the ModifyTable node counts its loops here and no rows (no RETURNING).
//!
//! The row op as a step (face/end.rs `RowOpMech`, M4's write kinds) is write/rowop_step.rs, over this file's `claim`,
//! `store_pos` and `host_op`: M1's DELETE end emits the same IR as before the split.
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE, WRITE_RECHECK};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{ArenaVec, Cmd, Parallel, SiteDesc, WritePlan};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::end::{EndMech, SinkKind};
use crate::face::kinds::{Face, KindId};
use crate::face::source::Locator;
use crate::frame::env::Env;
use crate::frame::instr;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::rerun::epq::marks::Marks;
use crate::admit::refusal::Reason;

/// What a row op's kind hands the end: the host op (`WRITE_DELETE` ...), the target relation, the cells its write site
/// carries (0 for DELETE: the position alone) and the frozen write plan the driver binds the write host from.
pub(crate) struct RowOp {
    pub op: u32,
    pub relid: u32,
    pub natts: u16,
    pub plan: WritePlan,
}

pub(crate) struct RowOpEnd<'g> {
    op: u32,
    relid: u32,
    /// The write site (`SiteDesc::write`), numbered at the end's make as M1 numbered it (its 21 IR pins).
    site: Late,
    /// The pipeline's node (refusals) and the ModifyTable node (its EXPLAIN ANALYZE loops).
    node: i32,
    write_node: i32,
    /// D655: the joined relations' marks, stored as their sites' positions before the host call.
    marks: Marks<'g>,
}

/// A site number, placed at the make or at emit (`Px::place_late_sites`), read while lowering.
pub(crate) type Late = std::rc::Rc<core::cell::Cell<usize>>;

/// The write site added and the program's write plan set for pipeline `k`: (site, the pipeline's node, the ModifyTable
/// node). `late`: numbered at emit after every source site (the step form: exec admit's host registry is positional over
/// the source sites, and a write over a join makes another pipeline's source after it); else now (M1's DELETE end over
/// its own scan, whose pinned IR numbers it first). D1687: a writing CTE's row op beside the statement's own write (or
/// beside an earlier writing CTE's under a SELECT) is a write plan of its own (`Descs::writes`, by its write site, which
/// the driver's writer for it answers), while `two_plans_served` holds; any other second row op refuses by the kind
/// that serves a write feeding its statement, `nest.cte` (M8; Decision 5).
pub(crate) fn claim<'g>(px: &mut Px<'g, '_>, k: usize, relid: u32, natts: u16, plan: WritePlan, late: bool) -> (Late, i32, i32, Marks<'g>) {
    // a data-modifying CTE's write sets no tag: the statement's is the main statement's (the CTE-write seam)
    let plan = WritePlan { cte: px.gx.cte_write(k), ..plan };
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    if matches!(p.desc.parallel, Parallel::Eligible) {
        crate::refuse!(node, Reason::Bug { payload: "a row op on a parallel-eligible pipeline (decompose makes a write serial)" });
    }
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    let write_node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= wi).map_or(node, |n| n.node_id);
    // an arm of the Append a write of several result relations streams into (write.route, M4-ROUTE R2): the arms write
    // through one site, the first arm's, numbered late (every arm's source site comes first), under the one plan
    let arm = p.ops[..wi].iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
    // D3681: or the one pipeline reading every result relation as a relation set (decompose ops/append.rs `relsets`): each
    // row's position names its own relation (`store_loc`)
    let set = set_source(&p.source);
    // D652 (D610's rest): the leader's recheck in a program of several pipelines (exec epq::recheck_leader) reruns the
    // producers the stopped pipeline reads only when its marks name them (D655, marks.rs: an UPDATE / DELETE / MERGE over
    // a join; M4-MERGE R2), an INSERT never rechecks; Append arms beside their siblings (write.route, M4-ROUTE R2) read none
    let w = p.ops.get(wi).and_then(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let mut epq_ir = false;
    let marks = match plan.cmd {
        // V4R-353, D886: a write into a view never rechecks (its INSTEAD OF triggers are the write: no heap version, no
        // EvalPlanQual; hosts view_write never answers Recheck): its marks are never read
        Cmd::Update | Cmd::Delete | Cmd::Merge if w.is_some_and(|w| w.members.has(gravity_decompose::ops::shared::modifytable::Member::TargetView)) => Marks::none(px.mcx()),
        // mergecmd insert-only MERGE: no MATCHED action, so no row is locked or rechecked (PostgreSQL's ExecMergeMatched
        // returns at once without one; nest_write.rs `positionless`): its marks are never read
        Cmd::Merge if w.is_some_and(|w| w.merge.as_ref().is_some_and(|m| super::nest_write::positionless(w, m))) => Marks::none(px.mcx()),
        // D655 Row C (V4R-534): a data-modifying CTE's one-site write rechecks in the IR (write/rowop_epq.rs): no marks; D2297
        // (round 4): its plan says so (`WritePlan::epq_ir`)
        Cmd::Update | Cmd::Delete | Cmd::Merge if super::rowop_epq::in_ir(px.gx, k) => {
            epq_ir = true;
            Marks::none(px.mcx())
        }
        // producerchain: a nest inner's REFERENCE row is positioned at its RowOf site (write/nest_ref.rs, lockrows NEST's form)
        // C4 (R83, D2050): the producers its recheck reruns are asked of the write road's admission (rerun/epq/write_road.rs)
        Cmd::Update | Cmd::Delete | Cmd::Merge => {
            // D3605: or a producer's nest inner's (nest_write.rs `producer_inners`)
            let inners = w.is_some_and(|w| super::nest_ref::chain(px.gx, k, w) || super::nest_write::dml_chain(px.gx, k, w) || super::nest_write::producer_inners(px.gx, k, w));
            let marks = crate::rerun::epq::marks::sites_in(px, k, node, w.map_or(&[][..], |w| &w.mark_descs[..]), &|rti| w.is_some_and(|w| w.targets.iter().any(|t| t.rti == rti)), inners);
            if let Some(w) = w {
                let targets: PgVec<'_, u32> = w.targets.iter().map(|t| t.rti).collect_in(px.mcx());
                crate::rerun::epq::write_road::admit(px, k, node, w.mark_descs, &targets[..]);
            }
            marks
        }
        _ => Marks::none(px.mcx()),
    };
    let plan = WritePlan { epq_ir, ..plan };
    // D2489: an arm of a write over several result relations brings the shared plan without its leaves (write.route's
    // `target`); the first arm's claim adds them once, every later arm's plan matches it but for them
    let several = (arm || set) && w.is_some_and(|w| w.targets.len() > 1);
    // D1687: the statement's own write is the program's `write` (its tag and row count, the runner's recheck); a writing
    // CTE's beside it, or beside an earlier writing CTE's under a SELECT, is one of `writes`, by its own write site. A
    // program `two_plans_served` refuses keeps the one-plan claim order: the refusal is the second claim's, after the
    // makes between them (their own refusals by name come first, as before D1687)
    // D3276 (DRAINED LOCK): or a drained CTE's write beside the statement's row lock (`lock_beside_drained`), which then
    // claims the program's plan as the statement's own
    let beside = px.write_plan().is_some() || plan.cte && (main_writes(px.gx) && two_plans_served(px.gx) || px.gx.lock_beside_drained());
    if beside {
        let shared = px.ext::<Arms>(Face::RowOp).filter(|c| arm && c.write_node == write_node && px.write_plan().is_some_and(|s| shares(s, &plan, several)));
        if let Some(c) = shared {
            return (c.site.clone(), node, write_node, marks);
        }
        // a second write outside every CTE (two row ops of the statement's own) keeps the refusal
        if arm || !plan.cte || !two_plans_served(px.gx) && !px.gx.lock_beside_drained() {
            crate::not_built!(node, crate::admit::refusal::Variant::Kind(KindId::NestCte))
        }
        // D2605 PULLED: unless every writing CTE is pulled by its readers, each ModifyTable's statement triggers then firing at
        // its own first call and end (`pulled_ends`); D2605 EARLY: unless the CTE's end is the own write pipeline's, which
        // then queues its AFTER STATEMENT events (early_as.rs)
        let pulled = pulled_ends(px.gx);
        if !trigger_order_kept(px.gx) && !pulled && !super::early_as::declare(px) {
            crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("two write plans whose targets both fire statement or AFTER ROW triggers, beside a writing CTE the statement reads or under a SELECT (PG queues a writing CTE's AFTER STATEMENT events when its ModifyTable ends: D2605)") });
        }
        // D2772 POST: unless the CTE's pipelines run after the statement's own, its writer bound at the turn (post.rs)
        if apart_order_lost(px.gx) && super::post::set(px.gx).is_none() && !pulled {
            crate::refuse!(node, Reason::Shape { why: APART_ORDER });
        }
        if !main_writes(px.gx) && px.gx.lock_beside_drained() {
            fires::fire("write.lock_beside_drained"); // shapecheck fire signal (D3276 DRAINED LOCK): a drained CTE's write beside the lock's plan
        }
        let site = px.add_late_site(SiteDesc::write(relid, natts));
        px.writes.push((site.clone(), write_node, plan, k)); // alloc: Program
        return (site, node, write_node, marks);
    }
    // D1710: M1's DELETE end would number its site now, but an InitPlan's pipeline (W5a: exempt from D652) is made after
    // it, and exec admit's host registry is positional over the source sites, so a statement with an InitPlan numbers the
    // write site late (the InitPlan's source site comes first and finds its recipe)
    let init = px.gx.graph.init_plans.iter().any(|i| i.pipeline as usize != k && !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte));
    // D655: a write over a join numbers its site late too (the other pipelines' source sites come first); D2772 POST: so
    // does the statement's own write beside post pipelines (the CTE's source sites are numbered before its own); D3278
    // (DRAINED): and beside drained CTEs (made first, their sources numbered before its own); D3643: and a write whose
    // recheck scans a union's members again (no position stored, their sites still numbered before its own scan's)
    let over_join = !marks.pos.is_empty() || !marks.rescan.is_empty();
    let site = match late || arm || set || init || over_join || super::post::apart(px.gx, k) || !plan.cte && ctes_drained(px.gx) {
        true => px.add_late_site(SiteDesc::write(relid, natts)),
        false => std::rc::Rc::new(core::cell::Cell::new(px.add_site(SiteDesc::write(relid, natts)))),
    };
    if arm {
        *px.ext_mut::<Arms>(Face::RowOp) = Arms { write_node, site: site.clone(), old: None, sys: None };
    }
    let mut plan = plan;
    if let (true, Some(w), Some(r)) = (several, w, plan.route.as_mut()) {
        let leaves = crate::ops::modifytable::strategies::route::leaves(px.gx, w);
        r.leaves = ArenaVec::with_capacity_in(leaves.len(), px.arena);
        r.leaves.extend_from_slice(&leaves);
    }
    px.keep_write(plan);
    px.write_k = Some(k);
    (site, node, write_node, marks)
}

/// D1687: a pipeline outside every CTE writes (the statement's own ModifyTable).
pub(crate) fn main_writes(gx: &crate::graph::Gx<'_, '_>) -> bool {
    gx.pipelines().iter().enumerate().any(|(k, p)| !gx.cte_write(k) && p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd != Cmd::Select)))
}

/// D1687 (fail closed): a statement of several write plans serves only when no row lock is beside them (D3488 CTECLAIM:
/// but a writing CTE's job-queue claim, its lock rechecked in its own IR, rowop_epq.rs `claim_lock`) and each write is
/// an INSERT (D2644: ON CONFLICT too; it never stops for EvalPlanQual, the host restarts its arbiter check, and a beside
/// writer answers its DO UPDATE arm at its conflict site, driver dml.rs `owns`) and no MERGE, with no check option (a
/// view's CHECK OPTION, a policy's WITH CHECK) beside ON CONFLICT (D3480: a plain INSERT's check options are served),
/// or a writing CTE's UPDATE / DELETE that rechecks in its own IR (write/rowop_epq.rs, D655 Row C): no writer then
/// stops for the runner's EvalPlanQual, which reruns the producers (a writing CTE's write among them) and knows one
/// write site. lockrows CTECOPY: or the statement's own UPDATE / DELETE (no check option, no MERGE) whose recheck
/// reruns no writing CTE (`ctes_copied`); its MERGE beside a writing CTE stays refused, but one whose actions only
/// insert or do nothing (it never rechecks). D655 Row C: or a writing CTE's
/// MERGE that rechecks in its own IR (merge/epq.rs `beside`), as such an UPDATE / DELETE does. D1687: or a writing CTE's
/// MERGE whose actions only insert or do nothing, a check option or not: it never rechecks, and its writer reads its own
/// check options and MERGE subcommands, not the program write's (exec ops/modifytable/beside.rs).
/// A writing CTE's write that never stops for the runner's EvalPlanQual: an INSERT with no check option, an UPDATE /
/// DELETE that rechecks in its own IR (D655 Row C), a MERGE that does (merge/epq.rs `beside`) or only inserts.
pub(crate) fn cte_write_alone(gx: &crate::graph::Gx<'_, '_>, k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
    let insert = w.cmd == Cmd::Insert && w.checks.is_empty() && w.merge.is_none();
    let in_ir = matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.checks.is_empty() && gx.cte_write(k) && super::rowop_epq::in_ir(gx, k);
    let merge_inserts = w.cmd == Cmd::Merge && gx.cte_write(k) && w.on_conflict.is_none() && w.merge.as_ref().is_some_and(super::merge::nm::insert_only);
    insert || in_ir || super::merge::epq::beside(gx, k, w) || merge_inserts
}

fn two_plans_served(gx: &crate::graph::Gx<'_, '_>) -> bool {
    // D3480: an INSERT without ON CONFLICT may carry check options (a view's WITH CHECK OPTION, a policy's WITH CHECK):
    // they run on its new row in its own writer's write_row call (strategies/checks_ir.rs, hosts write_check.rs; a writer
    // beside the program write reads its own, exec ops/modifytable/beside.rs), as a writing CTE's insert-only MERGE
    // through a checked view's do (`cte_merge_inserts`); an INSERT never rechecks
    let insert = |w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| w.cmd == Cmd::Insert && w.merge.is_none() && (w.checks.is_empty() || w.on_conflict.is_none());
    let cte_rechecks_in_ir = |k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.checks.is_empty() && gx.cte_write(k) && super::rowop_epq::in_ir(gx, k);
    // D2772 POST (triggers#500): or one beside the one data-modifying CTE nothing reads, whose pipelines run after the
    // statement's own and which its recheck never reruns (post.rs `apart`)
    // D3278 (DRAINED): or one beside writing CTEs nothing reads, each with a RETURNING list (stored, drained after the
    // statement's emit), which its recheck never runs (rerun/epq/marks.rs `drained`)
    // D3484 (INITCTE): or one beside writing CTEs only the statement's non-CTE InitPlans read, each a trigger-free INSERT
    // (D4673: or UPDATE / DELETE rechecking in its own IR) (rerun/epq/inits.rs `init_ctes`): PostgreSQL runs such a CTE when the InitPlan first reads it (ExecPostprocessPlan
    // finishes it), and the write's EvalPlanQual copies the InitPlan's value (EvalPlanQualStart), running neither again
    // (the InitPlan's members are `recheck_skip`, frame/program.rs), as D3379's row lock beside them
    let own_copies = |k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.checks.is_empty() && w.merge.is_none() && !gx.cte_write(k) && (ctes_copied(gx, k, w) || super::post::apart(gx, k) || ctes_drained(gx) || ctes_init(gx));
    // CTE-MERGE (triggers#863): or the statement's own MERGE whose only actions insert or do nothing (nest_write.rs
    // `positionless`): PostgreSQL's ExecMergeMatched never runs for it, so it never stops for an EvalPlanQual either
    let merge_inserts = |k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| w.cmd == Cmd::Merge && w.checks.is_empty() && w.on_conflict.is_none() && !gx.cte_write(k) && w.merge.as_ref().is_some_and(|m| super::nest_write::positionless(w, m));
    // D655 Row C: or a writing CTE's MERGE that rechecks in its own IR (merge/epq.rs `beside`)
    let cte_merge_in_ir = |k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| super::merge::epq::beside(gx, k, w);
    let cte_merge_inserts = |k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| w.cmd == Cmd::Merge && gx.cte_write(k) && w.on_conflict.is_none() && w.merge.as_ref().is_some_and(super::merge::nm::insert_only);
    gx.pipelines().iter().enumerate().all(|(k, p)| {
        p.ops.iter().all(|o| match o {
            Op::Write(w) => insert(w) || cte_rechecks_in_ir(k, w) || own_copies(k, w) || merge_inserts(k, w) || cte_merge_in_ir(k, w) || cte_merge_inserts(k, w),
            // D3488 CTECLAIM: or a writing CTE's job-queue claim, its lock and its write each rechecked in its own IR
            // D3507: or CTELOCK's claim beside the statement's write and data-modifying CTEs' writes rechecking in
            // their IR
            // D3506 SETLOCK: or a lock in a hashed SubPlan's plan, its own Program.locks entry rechecked in the IR
            Op::Lock(_) => super::rowop_epq::claim_lock(gx, k) || crate::rerun::epq::beside::cte_claim_beside(gx, k) || crate::rerun::epq::set_lock::locks(gx).iter().any(|x| x.0 == k),
            _ => true,
        })
    })
}

/// D3484 (INITCTE): the statement's writing CTEs are rerun/epq/inits.rs `init_ctes`' (only non-CTE InitPlans read them,
/// each writes by trigger-free INSERT), at least one.
fn ctes_init(gx: &crate::graph::Gx<'_, '_>) -> bool {
    crate::rerun::epq::inits::init_ctes(gx).is_some_and(|c| !c.is_empty())
}

/// D3278 (DRAINED): every writing CTE's pipeline is one of rerun/epq/marks.rs `drained` (nothing reads it).
fn ctes_drained(gx: &crate::graph::Gx<'_, '_>) -> bool {
    let drained = crate::rerun::epq::marks::drained(gx);
    let ps = gx.pipelines();
    !drained.is_empty() && (0..ps.len()).filter(|&k| gx.cte_write(k) && ps[k].ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd != Cmd::Select))).all(|k| drained.contains(&k))
}

/// CTECOPY (D732 for a CteScan's row): the statement's own UPDATE / DELETE (write `w`) whose
/// recheck reruns no writing CTE: each writing CTE's pipeline is a producer that only the copy entry of a CteScan
/// reads, and `w`'s ROW_MARK_COPY mark carries that CteScan's row (`copy_row::upstream`: `recheck_skip`, so the driver
/// opens its slot empty). EvalPlanQual hands the CteScan the stored row (`u.*`), as PG's EvalPlanQualFetchRowMark does,
/// and never runs the CTE again. D3505 (graphile-worker's complete_job: `WITH j AS (DELETE FROM jobs .. RETURNING *)
/// UPDATE job_queues .. FROM j WHERE ..`, the CteScan the INNER of the write's Nested Loop): or the nest inner of the
/// write's pipeline `k` running that CteScan (rerun/epq/marks.rs `nest_copy`: a `nest_copy_scan` inner, whose re-open the
/// recheck hands the stored row alone, rerun/inner.rs `copy_walk`), the CTE's store read only through it (marks.rs
/// `only_through`: the owner consumes every slot its inners read, decompose breakers.rs `embed`), as `copy_pos` admits it.
fn ctes_copied(gx: &crate::graph::Gx<'_, '_>, k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
    use crate::rerun::epq::copy_row;
    let ps = gx.pipelines();
    let mut skipped: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    let copies = || w.mark_descs.iter().filter(|m| m.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY);
    let beside = copy_row::entries(gx, copies().map(|m| m.rti));
    for m in copies() {
        let Some(r) = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti) else { continue };
        // the pipeline running the CteScan: a producer, or (W5-C) the rechecking pipeline itself, as `marks::copy_pos` finds it
        if let Some((q, at)) = (0..ps.len()).find_map(|q| copy_row::boundary(gx, q, r).map(|at| (q, at))) {
            let up = copy_row::upstream(gx, q, at);
            if !copy_row::read_elsewhere(gx, q, at, &up, &beside) {
                skipped.extend_in(up);
            }
        } else if let Some(q) = crate::rerun::epq::marks::nest_copy(gx, k, m) {
            // D3505: the nest inner's copy entry (its boundary in that inner)
            let Some(at) = copy_row::boundary(gx, q, r) else { continue };
            let up = copy_row::upstream(gx, q, at);
            if !up.contains(&k) && (!copy_row::read_elsewhere(gx, q, at, &up, &beside) || crate::rerun::epq::marks::only_through(gx, k, q, &up)) {
                skipped.extend_in(up);
            }
        }
    }
    // the statement's own other write pipelines are its write's Append arms (one ModifyTable outside the CTEs)
    let writes = |c: usize| ps[c].ops.iter().any(|o| matches!(o, Op::Write(_)));
    (0..ps.len()).filter(|&c| gx.cte_write(c) && writes(c)).all(|c| skipped.contains(&c))
}

/// D2750 (fail closed): a statement's own write beside a writing CTE nothing reads whose trigger events the writers'
/// order would move (`apart_order_lost`).
const APART_ORDER: crate::admit::refusal::ShapeWhy = crate::admit::refusal::ShapeWhy("a writing CTE nothing reads beside the statement's own write, its BEFORE ROW or statement triggers beside the own target's BEFORE ROW triggers, or its AFTER ROW triggers beside the own target's AFTER ROW or statement triggers (PG runs the CTE's ModifyTable after the statement's own, from ExecPostprocessPlan; the served writers bind first: D2750)");

/// D2750 (fail closed; D1687's order): PostgreSQL runs a data-modifying CTE nothing reads once the statement's own
/// ModifyTable ends (ExecPostprocessPlan), so the CTE's BEFORE STATEMENT triggers fire after the own rows' BEFORE ROW
/// triggers and its row events after the own AFTER STATEMENT event is queued; the served writers all bind (BEFORE
/// STATEMENT) before any row, and their rows queue AFTER ROW events as they go. On box 57 `WITH c AS (UPDATE pq SET a =
/// a) INSERT INTO pp VALUES (1), (2)` served the CTE's BEFORE STATEMENT NOTICE ahead of pp's BEFORE ROW ones, which
/// PostgreSQL 18.6 prints after them (and triggers#500 the same). True when the statement has its own write and such a
/// CTE write's target fires, on its command, BEFORE ROW (INSTEAD OF) or statement triggers (they fire as the CTE runs)
/// while an own write's target fires BEFORE ROW (INSTEAD OF) ones (fired as the own rows go), or AFTER ROW triggers
/// (queued as the CTE's rows go, after the own AFTER STATEMENT event in PG) while an own target fires AFTER ROW or
/// statement ones (unknown facts fire). A CTE the statement reads keeps `trigger_order_kept`'s rule.
pub(super) fn apart_order_lost(gx: &crate::graph::Gx<'_, '_>) -> bool {
    use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
    use gravity_decompose::ops::shared::modifytable::WriteDesc;
    let ps = gx.pipelines();
    let writes: PgVec<'_, (usize, &WriteDesc<'_>)> = (0..ps.len()).flat_map(|k| ps[k].ops.iter().filter_map(move |o| if let Op::Write(w) = o { Some((k, w)) } else { None })).collect_in(gx.mcx);
    let unread = |k: usize| ps[k].desc.produces().is_none_or(|s| !ps.iter().any(|x| x.desc.consumes.contains(&s)));
    let fires = |w: &WriteDesc<'_>, f: &dyn Fn(TrigExt) -> u8| {
        // the plan's target, every result relation and a route's root (a partitioned root's statement triggers)
        let root = w.route.as_ref().map(|r| r.root_relid);
        core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid)).chain(root).any(|r| {
            let t = gx.facts.trig_ext(r);
            !t.known || f(t) & write_bits(w) != 0
        })
    };
    let own = writes.iter().any(|&(k, w)| !gx.cte_write(k) && w.cmd != Cmd::Select);
    let ctes: PgVec<'_, &WriteDesc<'_>> = writes.iter().filter(|&&(k, _)| gx.cte_write(k) && unread(k)).map(|&(_, w)| w).collect_in(gx.mcx);
    let cte_now = ctes.iter().any(|w| fires(w, &|t| t.before_row | t.instead_row | t.statement));
    let cte_queued = ctes.iter().any(|w| fires(w, &|t| t.after_row));
    let own_now = writes.iter().any(|&(k, w)| !gx.cte_write(k) && fires(w, &|t| t.before_row | t.instead_row));
    let own_queued = writes.iter().any(|&(k, w)| !gx.cte_write(k) && fires(w, &|t| t.after_row | t.statement));
    own && (cte_now && own_now || cte_queued && own_queued)
}

/// D1687 (fail closed): the writers bind in plan order, the statement's own first, so their BEFORE / AFTER STATEMENT
/// events keep PG's order when a writing CTE ends after the statement's ModifyTable (nobody reads it: it runs to its
/// end at ExecPostprocessPlan). A writing CTE the statement may read (its RETURNING) may end first, and under a SELECT
/// the CTEs' order is the drain's: kept only while at most one target relation fires statement or AFTER ROW triggers (or
/// its facts are unknown), or when the two writes are a one-row chain of data-modifying CTEs (`one_row_chain`, D2812).
fn trigger_order_kept(gx: &crate::graph::Gx<'_, '_>) -> bool {
    // D2776: the plan's target, every result relation and a route's root (a partitioned root's statement triggers fire on
    // the root, which `plan.target_rel` alone missed: an own UPDATE of a partitioned root beside a CTE it read served its
    // AFTER STATEMENT event ahead of the CTE's), as `apart_order_lost` reads them; a write keyed by its root
    let root = |w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| w.route.as_ref().map_or(w.plan.target_rel, |r| r.root_relid);
    let fires = |w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>| {
        core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid)).chain(w.route.as_ref().map(|r| r.root_relid)).any(|r| {
            let t = gx.facts.trig_ext(r);
            !t.known || (t.statement | t.after_row) & write_bits(w) != 0
        })
    };
    // a writing CTE with RETURNING may be read before the statement's ModifyTable ends (fail closed: read)
    let mut fired: PgVec<'_, (u32, bool)> = PgVec::new_in(gx.mcx);
    for (k, p) in gx.pipelines().iter().enumerate() {
        for o in p.ops.iter() {
            if let Op::Write(w) = o {
                if fires(w) {
                    fired.push_in((root(w), gx.cte_write(k) && w.returning.is_some()));
                }
            }
        }
    }
    if fired.iter().all(|&(r, _)| r == fired[0].0) {
        return true;
    }
    (main_writes(gx) && fired.iter().all(|&(_, read)| !read)) || one_row_chain(gx)
}

/// D2812 ONE-ROW CHAIN (fail closed): the statement's only two writes are data-modifying CTEs, `a` and `b`, where `a`
/// writes at most one row and every row `b` writes is one row of `a`'s RETURNING: `b`'s pipeline reads `a`'s product as
/// its source and nothing else, and only filters and projects it into its one write. PostgreSQL then queues every AFTER
/// ROW event of `a` before the first of `b` under any drive order (an InitPlan reading `a` first, or `b`'s CteScan pulling
/// `a` row by row): ExecUpdate / ExecDelete / ExecInsert queue a row's events before the ModifyTable hands the row on, and
/// `b` writes only rows it has read. The served writers run `a`'s pipeline before `b`'s, which reads its product, so the
/// AFTER ROW events keep that order. `a` writes at most one row when its pipeline is its target's own heap scan (range,
/// index or bitmap) and only filters, gates and projections before its write, whose equalities (`eq_cols`) fix every key
/// column of a unique, immediate, non-partial btree index (`unique_on`). Neither target fires statement or transition-table
/// triggers on its command (an AFTER STATEMENT event queues at each ModifyTable's end in PostgreSQL, at the statement's
/// end here: D2605); facts known; no route.
fn one_row_chain(gx: &crate::graph::Gx<'_, '_>) -> bool {
    use gravity_decompose::graph::KeyForm;
    use gravity_decompose::ops::shared::modifytable::WriteDesc;
    use gravity_decompose::{Am, Source};
    let ps = gx.pipelines();
    let writes: PgVec<'_, (usize, &WriteDesc<'_>)> = (0..ps.len()).flat_map(|k| ps[k].ops.iter().filter_map(move |o| if let Op::Write(w) = o { Some((k, w)) } else { None })).filter(|(_, w)| w.cmd != Cmd::Select).collect_in(gx.mcx);
    let quiet = |w: &WriteDesc<'_>| {
        let rels = core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid));
        w.route.is_none() && w.targets.len() <= 1 && w.merge.is_none() && rels.map(|r| gx.facts.trig_ext(r)).all(|t| t.known && (t.statement | t.transition) & write_bits(w) == 0)
    };
    // the ops before pipeline `k`'s one write, each `ok`
    let before = |k: usize, ok: &dyn Fn(&Op<'_>) -> bool| {
        let ops = &ps[k].ops;
        let n = ops.iter().filter(|o| matches!(o, Op::Write(_))).count();
        n == 1 && ops.iter().take_while(|o| !matches!(o, Op::Write(_))).all(ok) && ps[k].union.is_none()
    };
    let one_row = |k: usize, w: &WriteDesc<'_>| {
        let mut eqs: PgVec<'_, i16> = PgVec::new_in(gx.mcx);
        let rti = match &ps[k].source {
            Source::Index { am: Am::Heap, scanrelid, relid, keys, .. } if *relid == w.plan.target_rel => {
                eqs.extend_in(keys.iter().filter(|x| x.form == KeyForm::Op && x.attno > 0 && gx.facts.op_eq(x.opno)).map(|x| x.attno));
                *scanrelid
            }
            Source::Bitmap { am: Am::Heap, scanrelid, relid, recheck, .. } if *relid == w.plan.target_rel => {
                recheck.iter().for_each(|x| eq_cols(gx, x, *scanrelid, &mut eqs));
                *scanrelid
            }
            Source::Range { am: Am::Heap, scanrelid, relid, .. } if *relid == w.plan.target_rel => *scanrelid,
            _ => return false,
        };
        if u32::from(rti) != w.rti || !before(k, &|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) {
            return false;
        }
        for o in ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))) {
            if let Op::Filter(x) = o {
                eq_cols(gx, *x, rti, &mut eqs);
            }
        }
        crate::ops::shared::unique::unique_on(gx, w.plan.target_rel, &eqs[..])
    };
    let chain = |(ka, wa): (usize, &WriteDesc<'_>), (kb, wb): (usize, &WriteDesc<'_>)| {
        let Some(slot) = ps[ka].desc.produces() else { return false };
        let reads = matches!(ps[kb].source, Source::Breaker { slot: s } if s == slot) && ps[kb].desc.consumes.iter().all(|&x| x == slot);
        wa.returning.is_some() && reads && before(kb, &|o| matches!(o, Op::Filter(_) | Op::Project(_))) && one_row(ka, wa)
    };
    let &[x, y] = &writes[..] else { return false };
    gx.cte_write(x.0) && gx.cte_write(y.0) && quiet(x.1) && quiet(y.1) && (chain(x, y) || chain(y, x))
}

/// The attributes of scan `rti` that qual `x` fixes to one value (`one_row_chain`): the AND of `column = value` conjuncts
/// whose operator is a btree equality over non-collatable inputs and whose value is a constant or an external parameter.
fn eq_cols(gx: &crate::graph::Gx<'_, '_>, x: types_nodes::Node<'_>, rti: u16, out: &mut PgVec<'_, i16>) {
    crate::stack_check();
    use types_nodes::primnodes::{BoolExprType, ParamKind};
    if let Some(b) = x.as_bool_expr().filter(|b| b.boolop == BoolExprType::AND_EXPR) {
        b.args.iter().for_each(|a| eq_cols(gx, a, rti, out));
        return;
    }
    let Some(o) = x.as_op_expr().filter(|o| o.args.len() == 2 && o.inputcollid == 0 && gx.facts.op_eq(o.opno)) else { return };
    let (l, r) = (o.args.nth(0), o.args.nth(1));
    for (v, c) in [(l, r), (r, l)] {
        let fixed = c.as_const().is_some() || c.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN);
        if let Some(v) = v.as_var().filter(|v| fixed && v.varno == i32::from(rti) && v.varattno > 0 && v.varlevelsup == 0) {
            out.push_in(v.varattno);
        }
    }
}

/// D2644: the trigger events write `w` fires, as command bits: an INSERT ... ON CONFLICT DO UPDATE's UPDATE ones too (its
/// BEFORE / AFTER STATEMENT UPDATE, fireBSTriggers / fireASTriggers, and the conflict arm's BEFORE / AFTER ROW UPDATE).
fn write_bits(w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> u8 {
    use gravity_decompose::ops::modifytable::trig_facts::{cmd_bits, TRIG_UPDATE};
    cmd_bits(w.cmd) | if w.on_conflict.as_ref().is_some_and(|oc| oc.update) { TRIG_UPDATE } else { 0 }
}

/// D1687: the write plans beside the program's `write`, each by its write site (placed: `Px::place_late_sites` ran), and
/// D2605: where each ModifyTable ends, `write`'s last (`WriteAt::end`): under `pulled_ends` a writing CTE's at its pipeline's
/// program index (`at`, the graph pipeline's), every other at the statement's end. D3449: in a program whose post
/// pipelines `post.rs mark` flagged (`post`), a post CTE's writer binds at the turn before its group's first pipeline,
/// whose program index its `end` names (post.rs `start`).
pub(crate) fn writes(px: &mut Px<'_, '_>, at: &dyn Fn(usize) -> Option<usize>, post: bool, node: i32) -> (Vec<gravity_program::program::WriteAt>, u16) {
    use gravity_program::program::END_STATEMENT;
    let gx = px.gx;
    let pulled = pulled_ends(gx);
    let end = |k: usize| -> u16 {
        if post && gx.cte_write(k) {
            return match super::post::start(gx, k, at).and_then(|i| u16::try_from(i).ok()).filter(|&i| i != END_STATEMENT) {
                Some(i) => i,
                None => crate::refuse!(node, Reason::Bug { payload: "a post CTE's group has no program pipeline (D3449)" }),
            };
        }
        if !pulled || !gx.cte_write(k) {
            return END_STATEMENT;
        }
        match at(k).and_then(|i| u16::try_from(i).ok()).filter(|&i| i != END_STATEMENT) {
            Some(i) => i,
            None => crate::refuse!(node, Reason::Bug { payload: "a pulled writing CTE's pipeline is not the program's (D2605)" }),
        }
    };
    let writes = px.writes.iter().map(|(site, node, plan, k)| gravity_program::program::WriteAt { site: site.get() as u32, plan: plan.clone(), end: end(*k), node: *node }).collect(); // alloc: Program
    let write_end = px.write_k.filter(|_| px.write_plan().is_some_and(|w| w.cmd != Cmd::Select)).map_or(END_STATEMENT, end);
    (writes, write_end)
}

/// D2605 PULLED (regress with#234, triggers#499; fail closed): every ModifyTable of the statement fires its statement
/// triggers as PostgreSQL's does, at its own first call (fireBSTriggers) and end (fireASTriggers), because every writing
/// CTE is pulled by its readers (frame::iface `cte_pulled_write`: one row per miss, as PG's CteScan pulls its CTE, then
/// drained at the statement's end, ExecPostprocessPlan): its writer binds at its set's first pull and queues its AFTER
/// STATEMENT events when the set ran to its end; the statement's own write (outside every CTE) binds at the start and
/// queues them when the main plan ends, before the drain (driver node/ends.rs). Its rows' events queue as its rows go,
/// interleaved as the pulls interleave them. Asked only where `trigger_order_kept` or `apart_order_lost` would refuse and
/// EARLY (early_as.rs) does not serve; not beside post pipelines (post.rs) or a row lock, and only for INSERT / UPDATE /
/// DELETE writes (a MERGE's statement triggers may be another writer's: fail closed).
pub(crate) fn pulled_ends(gx: &crate::graph::Gx<'_, '_>) -> bool {
    let ps = gx.pipelines();
    let writes: PgVec<'_, (usize, &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>)> = (0..ps.len()).flat_map(|k| ps[k].ops.iter().filter_map(move |o| if let Op::Write(w) = o { Some((k, w)) } else { None })).filter(|(_, w)| w.cmd != Cmd::Select).collect_in(gx.mcx);
    let ctes = writes.iter().filter(|(k, _)| gx.cte_write(*k)).count();
    // D3462: no writing CTE, no pulled ends; the trigger-order tests below read every write's targets' trigger facts (a
    // relcache entry each, per target per write: a write over 4100 leaves lowered for minutes), so they are not asked
    if ctes == 0 {
        return false;
    }
    let locks = ps.iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Lock(_))));
    let plain = writes.iter().all(|(_, w)| matches!(w.cmd, Cmd::Insert | Cmd::Update | Cmd::Delete) && w.merge.is_none());
    let pulled = writes.iter().filter(|(k, _)| gx.cte_write(*k)).all(|&(k, _)| crate::frame::iface::cte_pulled_write(gx, k));
    let asked = !trigger_order_kept(gx) || apart_order_lost(gx);
    ctes > 0 && !locks && plain && pulled && asked && super::post::set(gx).is_none() && super::early_as::shape(gx).is_none()
}

/// D2489: a later arm's plan `p` is the plan the first arm stored (`s`): equal, but for the route's leaves when `several`
/// (the first arm's claim added them; `p` carries none).
fn shares(s: &WritePlan, p: &WritePlan, several: bool) -> bool {
    if !several {
        return s == p;
    }
    let WritePlan { target_rel, indexes, cmd, lock, wait, conflict, arbiters, route, old, sys, sys_header, cte, sys_old, epq_ir, checks_of, defer } = s;
    let route = match (route, &p.route) {
        (Some(a), Some(b)) => a.root == b.root && a.moves == b.moves && b.leaves.is_empty(),
        _ => false,
    };
    route && (*target_rel, cmd, lock, wait, conflict, *old, *sys, *sys_header, *cte, *sys_old, *epq_ir, *checks_of) == (p.target_rel, &p.cmd, &p.lock, &p.wait, &p.conflict, p.old, p.sys, p.sys_header, p.cte, p.sys_old, p.epq_ir, p.checks_of) && *defer == p.defer && *indexes == p.indexes && *arbiters == p.arbiters
}

/// The write site the first arm of a write over several result relations claimed, by its ModifyTable node, its old site
/// (V4R-350: RETURNING OLD of each arm's row) and its sys site (D774 x R5: its system columns), `shared_sys`.
#[derive(Default)]
struct Arms {
    write_node: i32,
    site: Late,
    old: Option<Late>,
    sys: Option<Late>,
}
crate::frame::px::ext_g!(Arms);

/// The sys site (`old`: the old site) after write site `site` (write/rowop_step.rs): an arm of a write over several result
/// relations shares the first arm's, as its write site (`claim`), so every arm's is the site after the one write site.
pub(crate) fn shared_sys(px: &mut Px<'_, '_>, site: &Late, relid: u32, cells: u16, old: bool) -> Late {
    let arm = px.ext::<Arms>(Face::RowOp).is_some_and(|a| std::rc::Rc::ptr_eq(&a.site, site));
    if let Some(s) = px.ext::<Arms>(Face::RowOp).filter(|_| arm).and_then(|a| if old { a.old.clone() } else { a.sys.clone() }) {
        return s;
    }
    let s = px.add_late_site(SiteDesc::write(relid, cells));
    if arm {
        let a = px.ext_mut::<Arms>(Face::RowOp);
        *(if old { &mut a.old } else { &mut a.sys }) = Some(s.clone());
    }
    s
}

/// The row-op end for pipeline `k` (M1's DELETE).
pub(crate) fn end<'g>(px: &mut Px<'g, '_>, k: usize, r: RowOp) -> PgBox<'g, dyn EndMech + 'g> {
    let (site, node, write_node, marks) = claim(px, k, r.relid, r.natts, r.plan, false);
    mcx::pg_box_dyn!(px.mcx(), RowOpEnd { op: r.op, relid: r.relid, site, node, write_node, marks } => dyn EndMech + 'g)
}

/// D3681: `source` is a relation-set site's (decompose ops/append.rs `join_twin`), whose rows each carry their own relation.
pub(crate) fn set_source(source: &gravity_decompose::Source) -> bool {
    matches!(source, gravity_decompose::Source::Range { site, .. } if site.set.len() > 1)
}

/// The write site's 12-byte position (`ScanPos`): flags | off << 16, the relation, the block (little-endian words).
pub(crate) fn store_pos(b: &mut FunctionBuilder<'_>, env: &Env, at: usize, relid: u32, block: Value, offset: Value) {
    store_pos_in(b, env, at, (relid, None), block, offset);
}

/// D3681: `store_pos` at a source's locator `l`: a relation-set source's row in its own relation (`Locator::rel`), else
/// `relid`.
pub(crate) fn store_loc(b: &mut FunctionBuilder<'_>, env: &Env, at: usize, relid: u32, l: &Locator) {
    store_pos_in(b, env, at, (relid, l.rel), l.block, l.offset);
}

/// `store_pos` in relation `rel` (an `I32`) when given, else `relid`.
fn store_pos_in(b: &mut FunctionBuilder<'_>, env: &Env, at: usize, (relid, rel): (u32, Option<Value>), block: Value, offset: Value) {
    let s = site_off(at) as i32;
    let off = b.conv(ConvOp::Trunc, Type::I32, offset);
    let sixteen = b.iconst(Type::I32, 16);
    let hi = b.int_bin(IntBinOp::Shl, off, sixteen);
    let flags = b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64);
    let w0 = b.int_bin(IntBinOp::Or, hi, flags);
    b.store(env.ctx, s + site::POS_FLAGS as i32, w0);
    let rel = rel.unwrap_or_else(|| b.iconst(Type::I32, relid as i32 as i64));
    b.store(env.ctx, s + site::POS_REL as i32, rel);
    let blk = b.conv(ConvOp::Trunc, Type::I32, block);
    b.store(env.ctx, s + site::POS_BLOCK as i32, blk);
}

/// `write_row(ctx, site, op)`: the host's status.
pub(crate) fn host_op(b: &mut FunctionBuilder<'_>, env: &Env, at: usize, op: u32) -> Value {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    let write_row = gravity_kernels::ops::modifytable::entries::write_row as *const () as usize;
    let h = b.declare_host_once("write_row", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), write_row, mem);
    let site_c = b.iconst(Type::I64, at as i64);
    let op_c = b.iconst(Type::I64, op as i64);
    b.call_host(h, &[env.ctx, site_c, op_c], Some(Type::I64)).unwrap_or(env.zero)
}

/// A stop at the row with `st` (the host's error, or the unsupported status): the rows before it counted.
pub(crate) fn stop_at(b: &mut FunctionBuilder<'_>, x: &Exits, row: &RowVals, st: Value) {
    b.br(x.l_stop_in, &[row.carry.out, row.at, st]);
}

impl EndMech for RowOpEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.write_node != self.node {
            instr::count_started(b, env.ctx, self.write_node);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let carry = Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) };
        let (l_cold, l_on) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_cold);
        let Some(loc) = env.loc else {
            // a source without a locator: only a Result under a constant-false gate reaches here (write/delete.rs admits no
            // other), so the row never comes; a write without a position is never emitted
            let t = b.bconst(true);
            b.cond_br(t, (l_cold, &[]), (l_on, &[]));
            b.switch_to(l_cold);
            let st = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
            stop_at(b, x, row, st);
            b.switch_to(l_on);
            return carry;
        };
        let Some(src) = env.site else { crate::refuse!(self.node, Reason::Bug { payload: "a row op whose source has a locator but no site" }) };
        store_loc(b, env, self.site.get(), self.relid, &loc);
        crate::rerun::epq::marks::store(b, env, row, self.node, &self.marks);
        let st = host_op(b, env, self.site.get(), self.op);
        // below zero: the host's error, the row not counted; above RECHECK (CONFLICT): no DELETE answers it
        let rc = b.iconst(Type::I64, WRITE_RECHECK);
        let odd = b.icmp(IntCond::Sgt, st, rc);
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let bad = b.int_bin(IntBinOp::Or, neg, odd);
        let l_rc_test = b.create_block(&[]);
        b.cond_br(bad, (l_cold, &[]), (l_rc_test, &[]));
        b.switch_to(l_cold);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        let err = b.select(neg, st, uns);
        stop_at(b, x, row, err);
        b.switch_to(l_rc_test);
        let is_rc = b.icmp(IntCond::Eq, st, rc);
        let l_rc = b.create_block(&[]);
        b.mark_cold(l_rc);
        b.cond_br(is_rc, (l_rc, &[]), (l_on, &[]));
        b.switch_to(l_rc);
        let rs = b.iconst(Type::I64, recheck(src));
        b.br(x.l_stop_in, &[out, row.next, rs]);
        b.switch_to(l_on);
        carry
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}
