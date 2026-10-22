//! V4R-527 (W6-D652-LOCK-INITPLAN; owner M8-WRITE): a row lock's EvalPlanQual beside an InitPlan, as PostgreSQL 18's.
//! EvalPlanQualStart (execMain.c:3076) forces each pending InitPlan attached at the LockRows / ModifyTable node or above it
//! that the recheck plan reads (ExecSetParamPlanMulti over its extParam) and copies the values, never the execPlan links:
//! such an InitPlan never runs again. One attached below (inside a SubPlan's plan, or at a node under the lock) is re-armed
//! in the EPQ estate (:3122; execProcnode.c:402-412 -> nodeSubplan.c:874) and evaluated again where the recheck reads it.
//! `admit` (marks.rs sites(), a lock's runner recheck): every non-CTE InitPlan of the program is attached at the lock's
//! node or above it (decompose `InitPlan::attach` among the rechecking pipeline's nodes from the lock's on), else
//! INIT_BELOW; and one of its params is read by that pipeline's own filters below the lock at an unconditional position
//! (each qual conjunct is its own `Op::Filter`, in ExecQual's order; inside one, decompose `walk_cond`'s rule), so every
//! row that reaches the lock has forced it, else INIT_READ (read only above the lock, as a LIMIT / OFFSET bound, only in
//! the projection, a SubPlan op, a build or at a conditional position, or never: the recheck could find it pending, where
//! PostgreSQL forces it). Its members are then exempt from D652 (marks.rs) and, the program locking, `recheck_skip`
//! (frame/program.rs): the runner's rerun never runs them and the stopped pipeline reads the Init cell this execution
//! evaluated, EvalPlanQualStart's copy. A write keeps W5a's exemption of every InitPlan; its recheck reuses an evaluated one
//! and forces a pending one its SubPlan reads (semantics D2028, exec rerun/epq.rs).
//! lockrows D2112 (phase A): `admit_ir` asks the same of a lock rechecked in the IR (V4R-512's product form, which
//! marks.rs never asks): served beside an InitPlan only where every row reaching the lock has forced it.
//! The M4 pair (plan.rs `Plan::role`, over W5 Row A's `Program.locks`): the RI temporal restrict check (ri_triggers.c:802-883)
//! locks twice, `LockRows(x)` over a filter reading InitPlan 1, whose `Aggregate` reads `LockRows(y)`; each LockRows keeps
//! its own ExecRowMarks (nodeLockRows.c). Served as exactly that pair: (A) the lock inside a non-CTE InitPlan's members in
//! V4R-512's product form (one site, feeding the Init's product: rechecked in the IR, never a runner stop inside the
//! Init's nested run) and (B) the emit's lock over one relation or a family's arms (admitted as above). Each is ONE
//! `LockSite` (A 1, B 2: `LockSite::arg`), its own lock host, so NOWAIT's 55P03, SKIP LOCKED, 40001 and WHERE CURRENT OF stay
//! per relation; B's arms share its entry, each arm on its own late lock site with its own leaf in POS_REL (L2's one family
//! host: PostgreSQL locks the row's one active child ExecRowMark, nodeLockRows.c). D4150 INITFAM: so do A's arms over a
//! family (the temporal-FK check over a partitioned PK table), each arm's lock the product form over its own leaf
//! (lower.rs `own_leaf`). D4154 INITMULTI: B of several marks over a join is marks.rs's form, its marks Program.locks
//! entries after A's; D4157 INITSORT: so is B over a breaker (ORDER BY), unless an AGA-35 bound form makes it (D3436,
//! B entry 2). D3775 COPYPAIR: or A the lock below the copy entry of a SubqueryScan whose row B's COPY mark carries, in a
//! product-making pipeline of its own (plan.rs `below_copy`): B's recheck reads that row stored, never running A again.
//! Refused by name: A of several lock ops and no family, or a family's arm in an embedded inner
//! (LOCK_INIT_FAMILY), A not in that form (LOCK_INIT_FORM), A over several relations or B in an embedded inner over
//! several (LOCK_INIT_MULTI), B of several marks another form makes (LOCK_INIT_UNCLAIMED),
//! the pair beside a data-modifying CTE (LOCK_INIT_WRITE: the locks' program carries no write host for the CTE's
//! write). Any other program of two lock nodes, or more (B's several lock ops without a family too), keeps NotBuilt
//! nest.cte (write/lockrows.rs: one write plan per program).
//! D3436 INITBOUND (the AGA-35 bounds, siblings_aga35_*_initplan): INITSINK, A may also end in its InitPlan's sink (an
//! EXPR / MULTIEXPR / ROWCOMPARE sublink's `(SELECT x .. FOR UPDATE)`: the rechecked row goes on to the sink as to a
//! product), and B may be an AGA-35 form's lock (15b, REFFETCH, REFNEST, FAMLIMIT: plan.rs
//! `Census::pair_inits` counts A's InitPlan out, `Plan::pair_entry` gives B entry 2, a REFERENCE fetch goes through B's
//! host). INITCTE, those
//! forms beside a data-modifying CTE only the InitPlans read (`init_ctes`: trigger-free INSERTs, run once; no recheck runs
//! them again), its writer one of `Program.writes` beside the lock's plan (modifytable rowop.rs `lock_beside_drained`).
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::subplan::{walk_cond, SubLink};
use gravity_decompose::ops::shared::scan::{BitmapPlan, TidForm};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode, Source};
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeTag};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// The named refusals (module doc).
pub const INIT_BELOW: ShapeWhy = ShapeWhy("a row lock beside an InitPlan attached below the lock (EvalPlanQual re-arms and re-evaluates it: D652)");
pub const INIT_READ: ShapeWhy = ShapeWhy("a row lock beside an InitPlan its quals below the lock do not read unconditionally (EvalPlanQualStart forces a pending one: D652)");
pub const LOCK_INIT_FAMILY: ShapeWhy = ShapeWhy("a row lock beside an InitPlan whose own row lock is over a partitioned or inheritance parent");
pub const LOCK_INIT_FORM: ShapeWhy = ShapeWhy("a row lock beside an InitPlan whose own row lock is not rechecked in the IR (a join, a SubPlan, no product: a runner recheck inside the Init's run)");
pub const LOCK_INIT_MULTI: ShapeWhy = ShapeWhy("a row lock beside an InitPlan's row lock where one of the two locks several relations");
/// D4157: the M4 pair's B of several marks that marks.rs should make (plan.rs `PlanState`, laid after A's) made by another
/// form, its marks numbered from the program's first entry over A's.
pub const LOCK_INIT_UNCLAIMED: ShapeWhy = ShapeWhy("a row lock beside an InitPlan's row lock that another form makes, its entries over the InitPlan's");
pub const LOCK_INIT_WRITE: ShapeWhy = ShapeWhy("a row lock beside an InitPlan's row lock in a statement holding a data-modifying CTE (the pair's program has no write host)");

/// The nodes whose every child PostgreSQL's interpreter evaluates before the node's own step (a strict call's arguments
/// too), or whose conditional children `walk_cond` marks (AND / OR, COALESCE, CASE, a row comparison). A read under any
/// other node (a SubPlan's test or arguments, an array coercion's element, a JSON behavior, a subscript) does not count.
const EVERY: &[NodeTag] = &[
    NodeTag::T_OpExpr, NodeTag::T_FuncExpr, NodeTag::T_BoolExpr, NodeTag::T_CoalesceExpr, NodeTag::T_CaseExpr, NodeTag::T_CaseWhen,
    NodeTag::T_RowCompareExpr, NodeTag::T_RelabelType, NodeTag::T_CoerceViaIO, NodeTag::T_CollateExpr, NodeTag::T_NullTest,
    NodeTag::T_BooleanTest, NodeTag::T_DistinctExpr, NodeTag::T_NullIfExpr, NodeTag::T_ScalarArrayOpExpr, NodeTag::T_MinMaxExpr,
    NodeTag::T_RowExpr, NodeTag::T_ArrayExpr,
    // D3842 (Hibernate's XmlHolder UPDATE: `SET aggregate = xmlelement(name "e", xmlconcat(xmlelement(name "theString",
    // (SELECT xmlagg(..) ..)), ..))`): ExecInitExprRec evaluates an XmlExpr's named arguments and arguments, every one,
    // before its EEOP_XMLEXPR step
    NodeTag::T_XmlExpr,
];

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

/// marks.rs sites(): the InitPlans' members (`all`, W5a's) a recheck of pipeline `k`'s write never reruns, or its lock's
/// at plan node `node` once `admit` passes.
pub fn members<'g>(px: &Px<'g, '_>, k: usize, node: i32, write: bool, all: fn(&Px<'g, '_>) -> PgVec<'g, usize>) -> PgVec<'g, usize> {
    if !write {
        admit(px, k, node);
    }
    all(px)
}

/// Every refusal by name of the module doc for pipeline `k`'s row lock at plan node `node`; nothing without an InitPlan.
pub(crate) fn admit(px: &Px<'_, '_>, k: usize, node: i32) {
    checks(px, k, node, false, None, Part::Own);
}

/// lockrows D2112 (phase A): the same refusals for a lock whose recheck runs in the IR (write/lockrows.rs: V4R-512's product
/// form), which marks.rs `sites` never asks, over each InitPlan the rechecked part may read (`may_read`: PG forces only
/// those in the subplan's extParam; witness lockrows-d2112-initplan). An InitPlan whose own pipelines hold the lock is not
/// beside it (the RI check's pair A: plan.rs `Plan::role`); a lock inside any InitPlan's or CTE's pipelines reading one refuses
/// INIT_BELOW: EvalPlanQualStart initializes every subplan in the recheck's estate, re-arming InitPlans attached there.
pub fn admit_ir(px: &Px<'_, '_>, k: usize, node: i32) {
    checks(px, k, node, true, None, Part::Own);
}

/// AGA-35 (lockrows 15b, plan.rs `breaker_limit`): `admit_ir` for a lock whose IR recheck reruns another pipeline's part,
/// pipeline `q`'s source and its ops before op `jq` (the producer below the lock's Sort: PostgreSQL's EvalPlanQual reruns
/// Sort -> scan over the test tuple). Each InitPlan that part may read is attached at the lock's node or above it, and
/// one of its params is read by that part's filters at an unconditional position: every row the Sort hands the lock passed
/// them, so PG's recheck copies an evaluated value (EvalPlanQualStart) and the IR's reads the Init cell this run filled.
pub fn admit_ir_at(px: &Px<'_, '_>, k: usize, node: i32, q: usize, jq: usize) {
    checks(px, k, node, true, Some((q, jq)), Part::Own);
}

/// AGA-35 form 2 (lockrows REFFETCH, lower.rs `ref_limit`): `admit_ir_at` over the producer's part of a REFFETCH join, its
/// hash probe read through its own expressions (hash clauses, outer keys, join qual): what the REFERENCE side's build part
/// reads is that part's own question, which make() asks of it with `admit_ir_at`. An InitPlan read only by the build part
/// or only above the lock (a LIMIT count) is then no read of the producer's part. AGA-35 form 3 (lockrows REFBUILD,
/// lower.rs `ref_build`): the same over the probe pipeline's part, the locked relation's build arms asked on their own.
pub fn admit_ir_ref(px: &Px<'_, '_>, k: usize, node: i32, q: usize, jq: usize) {
    checks(px, k, node, true, Some((q, jq)), Part::Probe);
}

/// AGA-35 forms 3 and 4: `admit_ir_at` over a family's part, pipeline `q`'s ops before op `jq`, read past a union emit's
/// slot source and the append markers (no run-time pruning): those hand rows on and read nothing. Form 3 (lockrows REFBUILD
/// over a family, lower.rs `ref_build`): the union's emit before its build, the parent's own filters, gates and projections
/// over the arms' rows (each arm asked on its own, `admit_ir_at`). Form 4 (lockrows FAMLIMIT, lower.rs `family_limit`):
/// each arm, its leaf's scan, filters, gates and projections before its feed into the Sort or the merge.
pub fn admit_ir_family(px: &Px<'_, '_>, k: usize, node: i32, q: usize, jq: usize) {
    checks(px, k, node, true, Some((q, jq)), Part::Family);
}

/// AGA-35 form 5 (lockrows REFNEST, rerun/epq/ref_nest.rs `form`): `admit_ir_at` over the outer part of a REFNEST join, the
/// lock's pipeline `q` before op `jq`, its nest read through its own expressions (join qual, NestLoopParams): the REFERENCE
/// relation's part (the nest's inner, or a Materialize's producer) is asked on its own, with `admit_ir_at`.
pub fn admit_ir_nest(px: &Px<'_, '_>, k: usize, node: i32, q: usize, jq: usize) {
    checks(px, k, node, true, Some((q, jq)), Part::Nest);
}

/// Which part of a recheck `checks` reads (`may_read`): a pipeline's own source and ops (`Own`); a hash probe's through its
/// own expressions (`Probe`, `admit_ir_ref`); a family's union emit or arm past a slot source and the append markers
/// (`Family`, `admit_ir_family`); a nest's through its own expressions (`Nest`, `admit_ir_nest`); a write's own source and
/// ops, its SubPlan steps through their test and args only (`Write`, rerun/epq/inits.rs `write_inits_read`: a subquery's
/// InitPlans are attached inside its plan).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Own,
    Probe,
    Family,
    Nest,
    Write,
}

/// `at`: the rechecked pipeline and its op bound when they are not the lock's own (`admit_ir_at`).
fn checks(px: &Px<'_, '_>, k: usize, node: i32, ir: bool, at: Option<(usize, usize)>, part: Part) {
    let lp = &px.gx.pipelines()[k];
    // the lock's node and the nodes above it in this pipeline (a Limit, the root): `node_ops` lists them source first
    let above = lp.node_ops.iter().position(|n| n.node_id == node).map_or(&[][..], |a| &lp.node_ops[a..]);
    let (p, j) = match at {
        Some((q, jq)) => (&px.gx.pipelines()[q], jq),
        None => (lp, lp.ops.iter().position(|o| matches!(o, Op::Lock(_))).unwrap_or(lp.ops.len())),
    };
    // D4150 INITFAM: a streamed union's arm is held with the others (`with_producers` follows a slot to its last arm only,
    // `run_arms`): a family's lock arms inside an InitPlan all run in its on-demand run
    let run = px.gx.run_arms(k);
    let holds = |i: &gravity_decompose::InitPlan<'_>| {
        let of = px.gx.with_producers([i.pipeline as usize]);
        of.contains(&k) || run.iter().any(|a| of.contains(a)) || at.is_some_and(|(q, _)| of.contains(&q))
    };
    let inside = ir && px.gx.graph.init_plans.iter().any(|i| holds(i));
    // lockrows M4 (lk-0061): one attached inside another InitPlan's pipelines (a min() sublink's MinMaxAgg InitPlan) is
    // read there alone; the recheck copies the outer one's value, so neither PG's nor the runner's recheck runs it again
    let nested = |i: &gravity_decompose::InitPlan<'_>| {
        let ps = px.gx.pipelines();
        px.gx.graph.init_plans.iter().filter(|o| o.plan_id != i.plan_id).any(|o| px.gx.with_producers([o.pipeline as usize]).iter().any(|&q| ps[q].node_id == i.attach || ps[q].node_ops.iter().any(|n| n.node_id == i.attach)))
    };
    let beside = |i: &&gravity_decompose::InitPlan<'_>| !matches!(i.link, SubLink::Cte) && if ir { !holds(i) && may_read(p, j, i.params, part) } else { !nested(i) };
    for i in px.gx.graph.init_plans.iter().filter(beside) {
        // D3272: one the recheck arms afresh (`arms`) is PostgreSQL's re-armed InitPlan, read where the recheck reads it
        if arms(px.gx, node, i) {
            continue;
        }
        if inside || !above.iter().any(|n| n.node_id == i.attach) {
            shape(node, INIT_BELOW);
        }
        // D4154 INITMULTI: or, for the M4 pair's B (marks.rs's runner recheck, plan.rs `PlanState`), in the build of an INNER
        // hash join below the lock, or below the breaker under it (D4157 INITSORT; `fed_reads`)
        let pair_b = !ir && crate::ops::lockrows::plan::multi_b(px, node);
        if !read_first(px.gx, p, j, i, false) && !forces(px.gx, node, i) && !unread(px.gx, node, i) && !(pair_b && fed_reads(px.gx, at.map_or(k, |a| a.0), j, i)) {
            shape(node, INIT_READ);
        }
    }
}

/// D4154 INITMULTI: whether an INNER hash join's probe before op `j` of pipeline `q` reads its build from a pipeline whose
/// filters read InitPlan `i` at an unconditional position (`read_first` over the whole build pipeline): every row that
/// reaches op `j` carries a build row that passed them, so it has forced the InitPlan, as PostgreSQL's Hash reads its
/// child whole before the join returns its first row (the temporal restrict pair's x hashed, its filter reading the
/// InitPlan first). D4157 INITSORT: or `q`'s source is a breaker every producer of which reads it so (its life and that
/// life's union arms, `run_arms`), as PostgreSQL's Sort reads its child whole before its first row.
fn fed_reads(gx: &Gx<'_, '_>, q: usize, j: usize, i: &gravity_decompose::InitPlan<'_>) -> bool {
    let ps = gx.pipelines();
    let reads = |b: usize| read_first(gx, &ps[b], ps[b].ops.len(), i, false);
    if let Source::Breaker { slot } = ps[q].source {
        if let Some(l) = gx.life_of(q, slot).map(|l| l.0 as usize) {
            if reads(l) && gx.run_arms(l).iter().all(|&a| reads(a)) {
                return true;
            }
        }
    }
    ps[q].ops[..j].iter().any(|o| match o {
        Op::Join(JoinDesc::Probe { slot, jointype: types_nodes::jointype::JoinType::JOIN_INNER, .. }) => {
            (0..q).rev().find(|&b| ps[b].desc.produces() == Some(*slot)).is_some_and(|b| read_first(gx, &ps[b], ps[b].ops.len(), i, false))
        }
        _ => false,
    })
}

/// D3271: whether the recheck of the lock at plan node `node` forces InitPlan `i` when it is still pending, as
/// PostgreSQL's EvalPlanQualStart and EvalPlanQualBegin do (ExecSetParamPlanMulti over the lock's subplan's extParam):
/// the lock is the statement's recheck node (decompose `recheck_node`: its top lock, else its one lock under a SubqueryScan
/// or in a CTE) and `i` one of decompose's `recheck_inits` (`plain_init`).
/// The runner's recheck forces those (exec rerun/epq.rs `forced`), an IR recheck at its start (`forced`, epq_ir.rs).
pub(crate) fn forces(gx: &Gx<'_, '_>, node: i32, i: &gravity_decompose::InitPlan<'_>) -> bool {
    gx.graph.recheck_node == node && gx.graph.recheck_inits.contains(&i.plan_id) && plain_init(gx, i)
}

/// D3271: whether the recheck of the lock at plan node `node` copies InitPlan `i`'s value and never reads it: `i` is attached
/// at the lock or above it (decompose `recheck_above`) and none of its params is in the lock's subplan `extParam` (not one of
/// `recheck_inits`), as a LIMIT count above the lock. PostgreSQL's EvalPlanQualStart then neither forces nor re-arms it,
/// and a pending one stays pending through the recheck.
fn unread(gx: &Gx<'_, '_>, node: i32, i: &gravity_decompose::InitPlan<'_>) -> bool {
    gx.graph.recheck_node == node && gx.graph.recheck_above.contains(&i.plan_id) && !gx.graph.recheck_inits.contains(&i.plan_id) && !gx.graph.epq_fresh.contains(&i.plan_id) && plain_init(gx, i)
}

/// D3272: whether InitPlan `i` is one the recheck of the lock at plan node `node` arms afresh, as PostgreSQL's
/// EvalPlanQualStart initializes the plan below the lock and every SubPlan in the recheck's estate, an InitPlan attached
/// there (at a node below the lock, inside a SubPlan's or a CTE's plan, a CTE's lock's own too) running again where the
/// recheck first reads it while the statement keeps its own value: decompose's `epq_fresh` for the statement's recheck node
/// (`recheck_node`), not a CTE's InitPlan, its own pipelines locking and writing nothing and none a rerun set's (a set's
/// member InitPlan is re-armed at every re-open instead, D1262). The runner's recheck arms those (exec rerun/epq.rs
/// `fresh_inits`, D2673), an IR recheck around its redo (`armed`, epq_ir.rs).
fn arms(gx: &Gx<'_, '_>, node: i32, i: &gravity_decompose::InitPlan<'_>) -> bool {
    !matches!(i.link, SubLink::Cte) && gx.graph.recheck_node == node && gx.graph.epq_fresh.contains(&i.plan_id) && plain_init(gx, i)
}

/// D3271: a lock's recheck runs a re-armed InitPlan (`INIT_EPQ_FRESH`) again after the statement's own run of it, so every
/// slot its members read that a pipeline outside them produces (a CTE's store: eval-plan-qual's `(SELECT id FROM cte1)` beside
/// a CTE's lock) is held through the program's last rechecking pipeline, as PostgreSQL's re-run reads the
/// statement's CTE store again (EvalPlanQualStart copies the CTE param naming its leader). A slot whose number another
/// pipeline produces again before that recheck cannot be held there: INIT_BELOW, as before.
pub(crate) fn hold_epq_reads(px: &Px<'_, '_>, mechs: &[crate::frame::frame::Mechs<'_>], pipelines: &mut [gravity_program::Pipeline], node: i32) {
    use gravity_kernels::ops::shared::object::entries::INIT_EPQ_FRESH;
    let gx = px.gx;
    let rechecking = |i: usize| mechs.get(i).is_some_and(|m| gx.pipelines()[px.after_of(m.k)].ops.iter().any(|o| matches!(o, Op::Lock(_) | Op::Write(_))));
    let Some(last) = (0..pipelines.len()).rev().find(|&i| rechecking(i)) else { return };
    for p in px.protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::Init && p.arg & INIT_EPQ_FRESH != 0).map(|p| p.pipeline as usize) {
        let members = gravity_program::protos::members_in(pipelines, &px.protos, p).members;
        for &x in members.iter() {
            let x = x as usize;
            for k in 0..pipelines[x].consumes.len() {
                let slot = pipelines[x].consumes[k];
                let Some(q) = (0..x).rev().find(|&q| pipelines[q].produces() == Some(slot)) else { continue };
                if members.contains(&(q as u16)) || last <= x {
                    continue;
                }
                if (q + 1..=last).any(|j| pipelines[j].produces() == Some(slot)) {
                    shape(node, INIT_BELOW);
                }
                pipelines[q].live_through = pipelines[q].live_through.max(last as i16);
            }
        }
    }
}

/// D3271 / D3272: InitPlan `i`'s own pipelines lock and write nothing (the M4 pair's InitPlan lock keeps its refusals) and
/// none is a rerun set's (D1262's re-armed InitPlan).
pub(crate) fn plain_init(gx: &Gx<'_, '_>, i: &gravity_decompose::InitPlan<'_>) -> bool {
    let ps = gx.pipelines();
    let own = gx.with_producers([i.pipeline as usize]);
    let held = gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| own.contains(&usize::from(m))));
    !held && !own.iter().flat_map(|&q| ps[q].ops.iter()).any(|o| matches!(o, Op::Lock(_) | Op::Write(_)))
}

/// D3272: the InitPlans an IR recheck of the lock at plan node `node` arms (`arms`): each one's handle word
/// (rerun/params.rs `init_handle`), first parameter and parameter count.
pub(crate) fn armed<'g>(px: &mut Px<'g, '_>, node: i32) -> PgVec<'g, (u16, i32, u32)> {
    let gx = px.gx;
    let mut out = PgVec::new_in(px.mcx());
    for i in gx.graph.init_plans.iter().filter(|i| arms(gx, node, i)) {
        out.push_in((crate::rerun::params::init_handle(px, i.param), i.param, i.params.len() as u32));
    }
    out
}

/// D3271: the InitPlans an IR recheck of the lock at plan node `node` forces (`forces`): each one's first parameter and
/// handle word (rerun/params.rs `init_handle`), by that parameter, the order ExecSetParamPlanMulti walks extParam in.
pub(crate) fn forced<'g>(px: &mut Px<'g, '_>, node: i32) -> PgVec<'g, (i32, u16)> {
    let gx = px.gx;
    let mut ids: PgVec<'_, i32> = gx.graph.init_plans.iter().filter(|i| !matches!(i.link, SubLink::Cte) && forces(gx, node, i)).map(|i| i.param).collect_in(px.mcx());
    ids.sort_unstable();
    let mcx = px.mcx();
    ids.into_iter().map(|id| (id, crate::rerun::params::init_handle(px, id))).collect_in(mcx)
}

/// INIT_READ's rule: whether pipeline `p`'s part before op `j` reads InitPlan `i` at an unconditional position, so every
/// row that reaches op `j` has forced it: an index scan's keys, a TID scan's TID quals, or a filter (each qual conjunct
/// its own `Op::Filter`, in ExecQual's order; inside one, decompose `walk_cond`'s rule). Also D2904's (rerun/epq/inits.rs),
/// with `proj`: a write's part also forces it in a projection below the write (ModifyTable's subplan projects every row it
/// hands on, ExecProject over its whole target list, so an UPDATE's SET expressions run before the write sees the row).
pub(crate) fn read_first(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, j: usize, i: &gravity_decompose::InitPlan<'_>, proj: bool) -> bool {
    // an index scan's keys run at its first fetch, before any row reaches the lock (lk-0061: `id = (SELECT min(id) ..)`)
    let mut read = match &p.source {
        Source::Index { keys, .. } => keys.iter().any(|k| key_reads(gx, k.value, i.params)),
        _ => false,
    };
    // so do a TID scan's TID quals (`ctid = (SELECT ctid ..)`, the TIDINIT form), every one (TidListEval at
    // the first TidNext skips none: a NULL is only no TID); a TID range scan stops at its first NULL bound, so not its
    let tid = match &p.source {
        Source::Tid { form: TidForm::Points, quals, .. } => Some(*quals),
        _ => None,
    };
    let mut below = PgVec::new_in(gx.mcx);
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(q) | Op::Gate(q) => below.push_in(*q),
            Op::Project(l) if proj => below.extend_in(l.iter().filter_map(|x| x.as_target_entry()).map(|te| te.expr)),
            _ => {}
        }
    }
    for q in tid.into_iter().flat_map(|l| l.iter()).chain(below) {
        walk_cond(q, false, &mut |x, cond| match x.as_param() {
            Some(r) => {
                read |= !cond && r.paramkind == ParamKind::PARAM_EXEC && i.params.iter().any(|id| id == r.paramid);
                false
            }
            None => EVERY.contains(&x.node_tag()),
        });
    }
    read
}

/// AGA-35 form 6 (lockrows SUBLOCK, lower.rs `sub_lock`): the lock at plan node `node` inside a SubPlan inner of pipeline `k`,
/// beside the outer query's InitPlans `sub_inits` counted out. Beside the statement's write (`write`), the write's own
/// recheck forces a pending one its plan reads (EvalPlanQualStart's ExecSetParamPlanMulti) and copies the value:
/// `admit_ir_at` over pipeline `k` up to its write, from the write's node, so each InitPlan is attached there or above
/// (INIT_BELOW) and read by the filters below the write (or an index scan's keys) at an unconditional position, every row
/// reaching the write having forced it (INIT_READ).
pub fn admit_sub(px: &Px<'_, '_>, node: i32, k: usize, write: bool) {
    let ps = px.gx.pipelines();
    if write {
        let p = &ps[k];
        let jw = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
        let wn = p.node_ops.iter().rev().find(|n| usize::from(n.first_op) <= jw).map_or(node, |n| n.node_id);
        checks(px, k, wn, true, Some((k, jw)), Part::Own);
    }
}

/// Whether PARAM_EXEC `id` is set by a non-CTE InitPlan of the statement. AGA-35 (REFFETCH, lower.rs `ref_limit`): the
/// REFERENCE side's ops rerun in the recheck over the refetched row with such a value copied from the main run
/// (EvalPlanQualStart), a bound word there once `admit_ir_at` has seen the build part read it unconditionally.
pub fn init_param(gx: &Gx<'_, '_>, id: i32) -> bool {
    gx.graph.init_plans.iter().any(|i| !matches!(i.link, SubLink::Cte) && i.params.iter().any(|p| p == id))
}

/// Whether index key `n` reads one of `params` before any row reaches the lock: the key itself, a plain run-time key, or
/// (D3654) a key expression that reads one at an unconditional position (`walk_cond`'s rule over `EVERY`, as a filter is
/// read) and calls nothing user or VOLATILE (`user_or_volatile`). ExecIndexEvalRuntimeKeys evaluates every run-time key
/// whole at the scan's first fetch. D3677: one that may raise (a data error: `(SELECT ..) - 295`'s int4mi) is taken too:
/// it is the host's key (params_key.rs `computed`, D1016), evaluated with PostgreSQL's evaluator once per open at the
/// site's seek (hosts ops/scan/param_key.rs `ParamKey::seek`), after the InitPlan's word is stored (frame/site.rs
/// `store_keys`), where PostgreSQL's first fetch evaluates it; the recheck reads the same InitPlan value (copied into
/// EvalPlanQual's estate), so a key that did not raise at the seek does not raise there.
fn key_reads(gx: &Gx<'_, '_>, n: Node<'_>, params: &types_nodes::list::IntList<'_>) -> bool {
    let ours = |exec: bool, id: i32| exec && params.iter().any(|p| p == id);
    if n.as_param().is_some_and(|r| ours(r.paramkind == ParamKind::PARAM_EXEC, r.paramid)) {
        return true;
    }
    let mut read = false;
    walk_cond(n, false, &mut |x, cond| match x.as_param() {
        Some(r) => {
            read |= !cond && ours(r.paramkind == ParamKind::PARAM_EXEC, r.paramid);
            false
        }
        None => EVERY.contains(&x.node_tag()),
    });
    read && crate::rerun::params_key::computed(n) && !crate::effects::user_or_volatile(gx, n)
}

/// lockrows D2112: whether pipeline `p`'s rechecked part (its source's keys and recheck quals, its ops below the lock op `j`)
/// may read one of `params`; any other source or op, or a node `expr::walk` does not know, may.
fn may_read(p: &PipelineNode<'_>, j: usize, params: &types_nodes::list::IntList<'_>, part: Part) -> bool {
    let hit = |n: Node<'_>| {
        let mut found = false;
        let known = gravity_decompose::expr::walk(n, &mut |x| match x.as_param() {
            Some(r) if r.paramkind == ParamKind::PARAM_EXEC && params.iter().any(|id| id == r.paramid) => {
                found = true;
                false
            }
            _ => true,
        });
        found || !known
    };
    fn bitmap(b: &BitmapPlan<'_>, hit: &dyn Fn(Node<'_>) -> bool) -> bool {
        crate::stack_check();
        match b {
            BitmapPlan::Index { keys, .. } => keys.iter().any(|k| hit(k.value)),
            BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().any(|b| bitmap(b, hit)),
        }
    }
    let src = match &p.source {
        Source::Range { .. } => false,
        Source::Index { keys, recheck, .. } => keys.iter().any(|k| hit(k.value)) || recheck.iter().any(hit),
        Source::Bitmap { plan, recheck, .. } => bitmap(plan, &hit) || recheck.iter().any(hit),
        // `Family` (`admit_ir_family`): a union emit's slot hands on the arms' rows, each arm asked on its own
        Source::Breaker { .. } if part == Part::Family => false,
        _ => true,
    };
    src || p.ops[..j].iter().any(|o| match o {
        Op::Filter(n) | Op::Gate(n) => hit(*n),
        Op::Project(l) => l.iter().any(hit),
        // `probe` (`admit_ir_ref`): a hash probe reads through its own expressions; its build part is checked on its own
        Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) if part == Part::Probe => clauses.iter().chain(keys.iter()).chain(qual.iter()).any(hit),
        // `Nest` (`admit_ir_nest`): a nest reads through its join qual and its parameters' values; its inner is asked on its own
        Op::Join(JoinDesc::Nest { params, qual, .. }) if part == Part::Nest => qual.iter().any(hit) || crate::rerun::epq::ref_nest::param_values(params).any(hit),
        // `Write` (rerun/epq/inits.rs): a SubPlan step reads this query level's InitPlans only through its test (an ANY /
        // ALL / row comparison's left side) and its args; its own subquery's InitPlans are attached inside its plan
        Op::SubPlan(s) if part == Part::Write => s.test.iter().any(|t| hit(*t)) || s.args.iter().any(hit),
        // `Family`: the append markers (no run-time pruning) read nothing
        Op::Append(AppendDesc::Union { prune: None, .. } | AppendDesc::Arm { prune: None, .. } | AppendDesc::Feed { .. }) if part == Part::Family => false,
        _ => true,
    })
}

/// D2904 (rerun/epq/inits.rs `write_inits_read`): `may_read` over a write's part before its op `j` (`Part::Write`).
pub(crate) fn write_may_read(p: &PipelineNode<'_>, j: usize, params: &types_nodes::list::IntList<'_>) -> bool {
    may_read(p, j, params, Part::Write)
}

/// D3379 INITSINK: pipeline `k` is a non-CTE InitPlan's own, ending in the sink that binds the Init's params (an EXPR,
/// MULTIEXPR or ROWCOMPARE sublink's `(SELECT x .. FOR UPDATE)`, which PostgreSQL's ExecSetParamPlan reads to its end or its
/// second row's raise). D3613: the program's one lock there (plan.rs `Plan::form`) is rechecked in the IR as the pair's A is:
/// its LockRows rechecks under its own EvalPlanQual inside the InitPlan's run, over the plan below it, which reads no
/// param of its own InitPlan, and hands the row on to the sink.
pub(crate) fn init_sink(gx: &Gx<'_, '_>, k: usize) -> bool {
    gx.pipelines()[k].desc.produces().is_none() && gx.graph.init_plans.iter().any(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Expr | SubLink::MultiExpr | SubLink::RowCompare))
}
