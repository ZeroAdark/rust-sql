//! D655 Row C (V4R-534; multilock plan 4.4 and its redo rules): EvalPlanQual in the IR
//! for a one-site UPDATE / DELETE whose pipeline produces for another, a data-modifying CTE (with RETURNING or none). The
//! runner's recheck reruns the stopped pipeline and sends its row to the client (marks.rs EMIT_ONLY); here the op redoes the
//! row where it stands, as ExecUpdate / ExecDelete's EvalPlanQual and redo_act loop. On WRITE_RECHECK the host has locked
//! the newest version (table_tuple_lock, FIND_LAST_VERSION); `WRITE_EPQ_ROW` puts it in the write site's cells and position
//! (hosts write.rs `epq_row`, its own slot pinned until the next fetch or unbind), the kind's recheck (write/epq_ir.rs: the
//! source's recheck quals and the ops below the write, the memo cleared so VOLATILE calls run again) skips the row when a
//! qual fails (its version's lock kept, EPQ's NULL), else forms the cells over it (UPDATE: the SET values over the newest
//! version, ExecGetUpdateNewTuple; DELETE: none) and calls the op again at that position. Its status goes back through the
//! op's own arms: DONE's one continuation (RETURNING over the row as written, OLD the version the retry replaced), SKIPPED,
//! a RECHECK again (redo_act), an error. The host's mapping stands for the retry: 40001 under a transaction snapshot,
//! TM_Deleted skipped, TM_SelfModified skipped at cmax = cid else 27000.
//! Admitted (`in_ir`, asked by write/rowop.rs `claim`, which then takes no marks): an UPDATE / DELETE of a data-modifying
//! CTE (with RETURNING or none: rowc-noret, whose redo writes the row and forms no output) over its one target's own heap
//! scan, only filters, gates and projections below it and no other relation's mark. Refused by name, in this order: a MERGE
//! (its recheck re-evaluates the WHEN clauses), several targets or a route, unknown trigger facts or a BEFORE ROW trigger
//! of the command on a write of several result relations (D2812 below), an InitPlan the ops below the write read
//! (EvalPlanQualBegin forces a pending one, and the first recheck's own plan tree runs it again: a VOLATILE one advances,
//! w6-rowc s2init); then epq_of's TID (D1672) and system column (ROWC_SYS) names. A LIMIT that may be zero, a negative
//! bound or a one-time gate elsewhere in the statement does not refuse: the CTE runs to its end, as PostgreSQL's
//! ExecPostprocessPlan runs it (D1688 / D1751, fixed). rowc-probe (modifytable, M4: `WITH u AS (UPDATE o SET .. FROM c WHERE c.id = o.cid ..)`):
//! INNER hash probes of REFERENCE-marked relations' own heap scans below the write too (`probed`): the redo matches the
//! newest version against the record each probe handed on (epq_ir.rs `Redo::Probe`), as EvalPlanQual joins it to the
//! REFERENCE row refetched at its junk ctid. rowc-copy (modifytable, M3 mt-0430: `d AS (DELETE FROM side WHERE id IN
//! (SELECT id FROM u))`, a Hash Semi Join over a CTE Scan): a SEMI probe too, and a build of a breaker's rows (a CTE's
//! store, a VALUES list) under a COPY mark, whose EvalPlanQual test tuple is the row the join read (`copy_built`); a
//! SEMI probe with a join qual past its hash keys too (D3379 SEMI-QUAL: its own first match, keys and qual, one PostgreSQL's
//! plan may pick: PG's is its bucket order's). D3397 ROWC-SUB
//! (modifytable_rowc_noret c3: `WITH d AS (DELETE FROM t WHERE t.v > (SELECT l.cap FROM l WHERE l.grp = t.grp) * 2) ..`):
//! eager per-row SubPlan steps among the filters and projections too (`subs_rowc`), each re-run at every call (what the
//! write's pipeline reads only its SubPlans' rerun sets make, none writing or locking), no InitPlan in the statement: the
//! redo evaluates each step again over the newest version (epq_ir.rs `epq_of_subs`, D652-S1's), as PostgreSQL's
//! EvalPlanQual re-runs the SubPlan with the new row's arguments. Another join, another SubPlan (conditional, hashed,
//! built once), another mark or another source stays marks.rs's EMIT_ONLY.
//! D652-S1 (modifytable, M2): the emit's UPDATE / DELETE over its one target's own heap scan whose only other pipelines are
//! the rerun sets of its per-row SubPlan steps (a correlated SubPlan over an aggregate inner, re-run at every call: the
//! runner's recheck would rerun those producers whole, D652) rechecks here too (`subs_in_ir`): the redo evaluates each
//! SubPlan step again over the newest version, in its place among the filters and projections (epq_ir.rs `epq_of_subs`),
//! as PostgreSQL's EvalPlanQual re-runs the SubPlan with the new row's arguments under the statement's snapshot. Any other
//! emit keeps the runner's recheck and its refusals (false, never a refusal of its own). writesubplan (D1112): a RETURNING
//! list's SubPlan steps stand after the write; what only they read (their rerun sets, a hashed set's build, an InitPlan
//! only the list reads) is no product of the recheck, as EvalPlanQual re-runs only the plan below ModifyTable. D3708
//! (lockrows_sublock :107): a BEFORE ROW trigger on the one result relation too, as D2812's and D3486's (`subs_form`).
//! D652-S2 (modifytable, M2): the same over an UPDATE / DELETE of several result relations (an unpruned partitioned or an
//! inheritance target, V4R-214: the write streams into each target's Append arm, over that target's own scan), its SubPlan
//! steps in every arm: each arm rechecks its own rows, the redo reading its own leaf's attributes (`epq_rel`) from the one
//! write site, which the multi-target host fills from the row's target's newest version (hosts write_route.rs `epq_row`).
//! The site is as wide as the widest target (`epq_width`). A cross-partition UPDATE (ExecCrossPartitionUpdate's own
//! EvalPlanQual) keeps the runner's recheck.
//! D652-H (modifytable, M4): a hashed SubPlan step among them (an uncorrelated `NOT IN` / `IN (SELECT ..)` that stayed a
//! SubPlan: `DELETE FROM t WHERE k NOT IN (SELECT ..)`) rechecks here too. Its table is a product the write's pipeline
//! reads, built once, by pipelines that are in no rerun set and neither write nor lock, before the write's first row; the
//! redo probes that table again over the newest version (site.rs `redo` over the probe step), where PostgreSQL's
//! EvalPlanQual rebuilds it in its own estate from the same plan under the statement's snapshot: a table kept where
//! PostgreSQL rebuilds, so the steps hold `Departure::Fewer`'s licence. D3678 (modifytable_d652h_hashed_recheck c7: `..
//! NOT IN (SELECT id FROM p WHERE random() >= 0.0)`): code that acts, a VOLATILE call, gets none, and the redo probes a
//! twin of each set instead (`write_twins`; D3506's subplan hashed.rs `twin_handle`, exec hash.rs `SubHashEpq`):
//! built at the redo's first probe by running the set's pipeline again, the call run again, and kept for the execution,
//! as EvalPlanQualStart's fresh SubPlan states build the set again and later rechecks reuse it; the statement's set is
//! never touched. Each set must be LAZY (its plan acts) and read no product (set_lock.rs `twin_set`), else the runner's
//! recheck, D652's refusal.
//! D652-I (modifytable, M3: `UPDATE p SET (a, b) = (SELECT .. WHERE c.pid = p.id), h = (SELECT max(..) FROM t)`): the
//! statement's own write beside InitPlans rechecks here too, their pipelines and producers left alone: the redo reads the
//! value the statement computed, as EvalPlanQualStart forces the InitPlans the EPQ tree reads in the parent estate and
//! copies their values. A statement with a CTE keeps the runner's recheck (w6-rowc s2init).
//! D2735 COPY-EMIT (modifytable, core-0274): the statement's own UPDATE / DELETE whose hash probes read a subquery's
//! product under a COPY mark rechecks here too, as rowc-copy (`copy_in_ir`): the runner's recheck would rerun the
//! subquery's plan, which EvalPlanQual never runs again (it substitutes the row the join read).
//! D2734 ROWC-UNION (modifytable, M3 gen mt-0354: `WITH d AS (DELETE FROM ph WHERE k = 26 RETURNING id, v) ..`, ph
//! partitioned): a data-modifying CTE's DELETE of several result relations is not streamed into their Append arms; it
//! reads one settled union of their scans, each row's relation its junk tableoid (`union_rows`). The redo is the row's
//! own relation's feed over its newest version, every other relation's scan blocked (EvalPlanQualStart's
//! relsubs_blocked), then the ops between the union and the write (epq_ir.rs `epq_of_union`); RETURNING reads the row's
//! own cells mapped onto the first target's attributes (write/delete_built.rs `ret_map`).
//! D2739 NEST-INNER (modifytable, subq-0323): a data-modifying CTE's DELETE whose target is the inner of an INNER nest
//! over a REFERENCE-marked relation's own scan rechecks here (`nest_inner`): the redo reruns the inner over the newest
//! version against the outer row the nest held, which is the refetched REFERENCE version (epq_ir.rs `epq_of_inner`).
//! D3647 BUILT-COPY (graphile-worker 0.17+ / 0.18.0's batched get_jobs, `q AS (UPDATE _private_job_queues .. FROM j ..)`
//! over fewer queues than the batch): a data-modifying CTE's UPDATE whose target's own scan is the build of an INNER hash
//! probe driven by a COPY-marked CTE's rows rechecks here too (`built_copy`): the redo runs the build's ops over the
//! newest version, forms the probe side again from the stored record and joins the two by the hash clauses and the join
//! qual (epq_ir.rs `epq_of_build_copy`), as EvalPlanQual reruns the Hash Join over its test tuple and the CTE Scan's
//! stored row.
//! D2812 BEFORE ROW (`WITH a AS (UPDATE t SET .. WHERE id = 400 RETURNING id), b AS (INSERT .. SELECT .. FROM a) ..`, t
//! with a BEFORE UPDATE ROW trigger): a write of one result relation whose target fires BEFORE ROW triggers on its
//! command rechecks here too. PostgreSQL's EvalPlanQual then runs in GetTupleForTrigger (trigger.c): it
//! locks the newest version (FIND_LAST_VERSION) before any trigger runs, rechecks it, forms the new row over it
//! (ExecGetUpdateNewTuple) and only then fires the triggers, OLD the locked version; the update itself cannot conflict
//! again. The host's BEFORE ROW prologue is that lock (hosts trigger_before.rs `lock_old`): a traversed lock answers
//! RECHECK with the newest version locked and no trigger run, the redo forms the cells over it and calls the op again at
//! its position, whose prologue takes the held lock (TM_Ok, not traversed) and fires the triggers over those cells. A
//! MERGE of one result relation takes the same lock in its action's prologue (merge/epq.rs, BEFORE ROW). D3373 (5)
//! ROWC-UNION-BR: so does a write of several result relations under the union form (ROWC-UNION), each row through its own
//! leaf's host (`rowc_tail_armed`'s `per_leaf`); a route host's other forms and ExecCrossPartitionUpdate's own delete leg
//! keep the refusal; unknown trigger facts too.
use gravity_abi::program::WRITE_EPQ_ROW;
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE, WRITE_DONE};
use gravity_decompose::ops::modifytable::trig_facts::cmd_bits;
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::effects::{self, Departure, Effects};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntCond, Type, Value};
use gravity_program::Cmd;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::end::EpqRerun;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

use super::rowop::{host_op, stop_at};

/// A qual's conjuncts read in place: its list's members, or the node itself (no list is built).
pub(crate) fn conjuncts<'p>(q: Node<'p>) -> impl Iterator<Item = Node<'p>> {
    let l = q.as_list();
    l.into_iter().flat_map(|l| l.iter()).chain(l.is_none().then_some(q))
}

/// The named refusals (module doc).
const ROWC_MERGE: ShapeWhy = ShapeWhy("a MERGE in a data-modifying CTE (its EvalPlanQual re-evaluates the WHEN clauses: D655 Row C)");
const ROWC_ROUTED: ShapeWhy = ShapeWhy("a rechecking UPDATE / DELETE in a data-modifying CTE through several targets or a partitioned / inheritance root (D655 Row C)");
const ROWC_BEFORE_ROW: ShapeWhy = ShapeWhy("a rechecking UPDATE / DELETE in a data-modifying CTE whose trigger facts are unknown, or a write of several result relations whose target fires a BEFORE ROW trigger (D655 Row C)");
const ROWC_INITPLAN: ShapeWhy = ShapeWhy("a rechecking UPDATE / DELETE in a data-modifying CTE whose ops below the write read an InitPlan (EvalPlanQual forces a pending one and its first recheck runs it again: D655 Row C)");

fn shape(node: i32, why: ShapeWhy) -> ! {
    crate::refuse!(node, Reason::Shape { why })
}

/// Whether pipeline `k`'s write op rechecks in the IR (module doc), else every refusal by name of the module doc; false
/// outside its scope (no data-modifying CTE, a view, an INSERT, several sites: marks.rs decides).
/// D3507 (lockrows plan.rs `Plan::write_pair`): pipeline `k` is a data-modifying CTE's UPDATE / DELETE with no check
/// option that rechecks in its own IR (`in_ir`).
pub(crate) fn cte_in_ir(gx: &Gx<'_, '_>, k: usize) -> bool {
    let write = gx.pipelines()[k].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    gx.cte_write(k) && write.is_some_and(|w| matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.checks.is_empty()) && in_ir(gx, k)
}

pub(crate) fn in_ir(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some((j, w)) = p.ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None }) else { return false };
    if p.desc.produces().is_none() {
        // D1933 (c) (admit): or the target's own scan the outer of one nested loop over a Material (nest_outer.rs); D2735:
        // or its hash probes of a COPY-marked breaker's rows (`copy_in_ir`); D2015 UNION-OUTER: or a MERGE through a root
        // over a LEFT nest of its leaves' arms whose outer is a union of constant rows (merge/nest_arms.rs `first`)
        // D3373 (4) UNION-SUBS: or a write of several result relations over a settled union of their feeds with per-row
        // SubPlan steps above the union (`union_subs`)
        // D3795: or the target a nest's inner past a nest over a merge or hash join's Material (`inner_in_ir`)
        return crate::ops::modifytable::merge::nest_arms::first(gx, k, w).is_some() || subs_in_ir(gx, k, j, w) || super::nest_outer::admits(gx, k, w) || copy_in_ir(gx, k, j, w) || union_subs(gx, k, j, w) || inner_in_ir(gx, k, j, w);
    }
    let scope = matches!(w.cmd, Cmd::Update | Cmd::Delete | Cmd::Merge) && !w.members.has(Member::TargetView);
    if !scope || !gx.cte_write(k) {
        return false;
    }
    let node = p.node_id;
    if w.cmd == Cmd::Merge {
        // D655 Row C: an insert-only MERGE never rechecks (ExecMergeMatched never runs without a MATCHED or BY SOURCE
        // action, and a NOT MATCHED INSERT has no position): no redo, outside this scope as an INSERT is
        if w.merge.as_ref().is_some_and(crate::ops::modifytable::merge::nm::insert_only) {
            return false;
        }
        // D655 Row C: the six forms merge/epq.rs redoes (its `admits`); every other MERGE by name
        let Some(exprs) = crate::ops::modifytable::merge::epq::admits(gx, k, j, w) else { shape(node, ROWC_MERGE) };
        return rowc_tail(gx, node, w, &[w.plan.target_rel], &exprs);
    }
    // D2734 ROWC-UNION (gen mt-0354): a DELETE of several result relations fed by one settled union (`union_rows`);
    // ROWC-UNION-UPDATE (triggers#500): or such an UPDATE
    if let Some(exprs) = union_rows(gx, k, j, w) {
        let rels: PgVec<'_, u32> = w.targets.iter().map(|t| t.relid).collect_in(gx.mcx);
        // D3373 (5) ROWC-UNION-BR: a leaf's BEFORE ROW trigger is that leaf host's GetTupleForTrigger leg (D2812), the
        // redo the row's own relation's (`epq_of_union`); a row moving to another partition keeps the refusal
        let moves = w.members.has(Member::CrossPartition) || w.route.is_some_and(|r| r.cross_partition);
        return rowc_tail_armed(gx, node, w, &rels, &exprs, &[], !moves);
    }
    let routed = [Member::Route, Member::Targets, Member::CrossPartition].into_iter().any(|m| w.members.has(m));
    if (w.targets.len() != 1 || w.route.is_some() || routed) && !pruned_leaf(w) {
        shape(node, ROWC_ROUTED);
    }
    // D3838 ROWC-INIT (Hibernate's CTE mutation with an InitPlan in SET: `update_cte_Animal (id) AS (UPDATE Animal SET
    // mother_id = (SELECT .. WHERE a2_0.id = 1) WHERE id IN (SELECT id FROM id_cte) RETURNING id)`): each form below
    // arms the InitPlans at the write's ModifyTable its first recheck runs again (rerun/epq/inits.rs `cte_write_armed`,
    // D3489's), its redo reading them as the own scan's does (epq_ir.rs `cte_armed`)
    let tail = |exprs: &[Node<'_>]| rowc_tail_armed(gx, node, w, &[w.plan.target_rel], exprs, &crate::rerun::epq::inits::cte_write_armed(gx, k, j), false);
    // D2739 NEST-INNER (subq-0323): or the target the inner of one INNER nest over a REFERENCE-marked relation's own scan
    if let Some((exprs, _)) = nest_inner(gx, k, j, w) {
        return tail(&exprs);
    }
    // D3650 NEST-COPY: or the target's own scan the outer of one INNER nest over a COPY-marked CTE's rows
    // (`copy_outer`)
    if let Some((exprs, _)) = copy_outer(gx, k, j, w) {
        return tail(&exprs);
    }
    // D3647 BUILT-COPY: or the target's own scan hashed, probed by a COPY-marked CTE's rows (`built_copy`)
    if let Some((exprs, _)) = built_copy(gx, k, j, w) {
        return tail(&exprs);
    }
    // D3824 DRAIN-COPY: or the target's own scan hashed under a RIGHT SEMI drain probed by a COPY-marked CTE's rows
    // (`drain_copy`)
    if let Some((exprs, _)) = drain_copy(gx, k, j, w) {
        return tail(&exprs);
    }
    // D3870 BUILT-HELD: or the target's own scan hashed, probed by a COPY-marked CTE's rows a group de-duplicates or a
    // DELETE's (`built_held`)
    if let Some((exprs, _)) = built_held(gx, k, j, w) {
        return tail(&exprs);
    }
    // D3619: or the statement's own nest form (write/nest_outer.rs: the target's own scan the outer of nests over
    // Materials, pg-boss resumeJobs' SEMI one over its id list), which takes no InitPlan but the CTE's own link
    if super::nest_outer::admits(gx, k, w) {
        return rowc_tail(gx, node, w, &[w.plan.target_rel], &[]);
    }
    let own = match &p.source {
        Source::Range { am, scanrelid, relid, .. } | Source::Index { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } | Source::Tid { am, scanrelid, relid, .. } => {
            *am == Am::Heap && *relid == w.plan.target_rel && u32::from(*scanrelid) == w.rti
        }
        _ => false,
    };
    // no other relation's mark, else rowc-probe's INNER hash probes of the REFERENCE-marked relations (`probed`)
    let marks = w.mark_descs.iter().all(|m| !m.parent && m.rti == w.rti);
    let exprs = below_in(gx.mcx, p, j).filter(|_| marks).or_else(|| probed(gx, k, j, w));
    // D3601 (2): no SubPlan in them but `probed`'s hashed steps (`stepped`); D3843 (Hibernate's SubRoot CTE UPDATE: `SET
    // name = (SELECT 'test' FROM Detail d1_0 WHERE d1_0.root_id = sr1_0.id)` over a Hash Join of the target's scan and
    // the id CTE) or per-row steps over their own inner (`row_sub`), which the redo runs again (`Redo::Sub`) as
    // EvalPlanQual re-evaluates the SubPlan in its fresh state
    let steps: PgVec<'_, i32> = p.ops[..j].iter().filter_map(|o| if let Op::SubPlan(d) = o { (set_step(d).is_some() || row_sub(gx, d)).then_some(d.plan_id) } else { None }).collect_in(gx.mcx);
    // D3397 ROWC-SUB: or eager per-row SubPlan steps among the ops below the write (`subs_rowc`)
    let exprs = exprs.filter(|x| !x.iter().any(|&e| gravity_decompose::expr::has_subplan(e) && !stepped(e, &steps))).or_else(|| subs_rowc(gx, k, j, w).filter(|_| marks));
    let Some(exprs) = exprs.filter(|_| own) else { return false };
    // D3489 CTECLAIM-INIT: the InitPlans at the write's ModifyTable its first recheck runs again (rerun/epq/inits.rs)
    let armed = crate::rerun::epq::inits::cte_write_armed(gx, k, j);
    rowc_tail_armed(gx, node, w, &[w.plan.target_rel], &exprs, &armed, false)
}

/// D3843: SubPlan step `d` runs at the row, never conditional, nested: over its own inner (a relation's scan under
/// filters and projections, reading no graph product and locking or writing nothing), or over its rerun set (an
/// aggregate's input: the set runs again at every call, as `subs_rowc`'s steps do), none of whose pipelines writes or
/// locks or reads a product made outside the set; no InitPlan attached inside its plan (EvalPlanQualStart arms one there again, where the redo would read the
/// statement's value). The redo opens it again over the rechecked row (epq_ir.rs `Redo::Sub`), its scans under the
/// statement's snapshot, as EvalPlanQual's fresh SubPlan state runs it.
fn row_sub(gx: &Gx<'_, '_>, d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>) -> bool {
    use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubSite};
    let SubForm::Nested { inner } = &d.form else { return false };
    let ps = gx.pipelines();
    let set: PgVec<'_, usize> = gx.graph.reruns.iter().filter(|r| r.plan_id == d.plan_id).flat_map(|r| r.pipelines.iter().map(|&m| usize::from(m))).collect_in(gx.mcx);
    let plain = set.is_empty() && inner.source.relation().is_some() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    // the set reads no product from outside it (a CTE's store, a build made once): the redo runs it whole again
    let sets = !set.is_empty() && set.iter().all(|&q| q < ps.len() && !writes_or_locks(gx, q)) && fed(gx, mcx::to_vec_in(&set[..], gx.mcx)).iter().all(|q| set.contains(q));
    let at = |i: &gravity_decompose::graph::InitPlan<'_>| inner.node_ops.iter().any(|n| n.node_id == i.attach) || set.iter().any(|&q| q < ps.len() && ps[q].node_ops.iter().any(|n| n.node_id == i.attach));
    let inits = gx.graph.init_plans.iter().all(|i| matches!(i.link, SubLink::Cte) || !at(i));
    d.site == SubSite::Row && !d.conditional && (plain || sets) && inits
}

/// D3397 ROWC-SUB (module doc): `below` for a data-modifying CTE's UPDATE / DELETE, write op `j` of pipeline `k`, whose ops
/// there are filters, gates, projections and eager per-row SubPlan steps (at the row, never conditional), at least one;
/// every pipeline the write's pipeline reads (with its feeders) a member of one of those steps' rerun sets, so re-run at
/// every call and never built once, none writing or locking; no SubPlan in the write's own expressions, no InitPlan in the
/// statement (a SubPlan's plan may read one: ROWC_INITPLAN's reason). The expressions below the write, the steps' args and
/// tests among them; else none (marks.rs EMIT_ONLY).
fn subs_rowc<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, Node<'p>>> {
    use gravity_decompose::ops::shared::subplan::{SubLink, SubSite};
    let ps = gx.pipelines();
    let p = &ps[k];
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.members.has(Member::WriteSubPlan) || gx.graph.init_plans.iter().any(|i| !matches!(i.link, SubLink::Cte)) {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = match &p.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().collect_in(gx.mcx),
        _ => PgVec::new_in(gx.mcx),
    };
    let mut ids: PgVec<'_, i32> = PgVec::new_in(gx.mcx);
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(q) | Op::Gate(q) => out.extend_in(conjuncts(*q)),
            Op::Project(l) => out.extend_in(l.iter()),
            Op::SubPlan(d) if d.site == SubSite::Row && !d.conditional => {
                ids.push_in(d.plan_id);
                out.extend_in(d.args.iter().chain(d.test));
            }
            _ => return None,
        }
    }
    let member = |q: usize| gx.graph.reruns.iter().any(|r| ids.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    let feeds = fed(gx, (0..ps.len()).filter(|&q| ps[q].desc.produces().is_some_and(|s| p.desc.consumes.contains(&s))).collect_in(gx.mcx));
    if ids.is_empty() || feeds.iter().any(|&q| q == k || !member(q) || writes_or_locks(gx, q)) {
        return None;
    }
    Some(out)
}

/// The refusals by name every Row C form takes last (module doc): unknown trigger facts on any of relations `rels` (the
/// write's), a BEFORE ROW trigger of the command on one of them when the write has several result relations (one
/// relation's is GetTupleForTrigger's leg, D2812, a MERGE's too), an InitPlan the recheck's expressions `exprs` read, a
/// LIMIT that may be zero or a one-time gate; else true.
fn rowc_tail(gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, rels: &[u32], exprs: &[Node<'_>]) -> bool {
    rowc_tail_armed(gx, node, w, rels, exprs, &[], false)
}

/// `rowc_tail`; D3489 CTECLAIM-INIT: the InitPlans `armed` (indexes in the graph's `init_plans`, rerun/epq/inits.rs
/// `cte_write_armed`) the redo runs again, as PostgreSQL's first recheck does, are no ROWC_INITPLAN read. D3373 (5)
/// ROWC-UNION-BR: `per_leaf`, several result relations under the union form, each row's BEFORE ROW leg its own leaf
/// host's (MultiTargetHost hands the row to its leaf's WriteHost, whose prologue locks the newest version and answers
/// RECHECK with that relation's position), as one relation's.
fn rowc_tail_armed(gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, rels: &[u32], exprs: &[Node<'_>], armed: &[usize], per_leaf: bool) -> bool {
    let one = rels.len() == 1 || per_leaf;
    for &relid in rels.iter() {
        let t = gx.facts.trig_ext(relid);
        if !t.known || (!one && t.before_row & cmd_bits(w.cmd) != 0) {
            shape(node, ROWC_BEFORE_ROW);
        }
    }
    // S2 and the first recheck's own InitPlan run (PG 18.6, w6-rowc s2init): no InitPlan (not a CTE) read below the write
    let rearmed = |id: i32| armed.iter().any(|&x| gx.graph.init_plans[x].params.iter().any(|q| q == id));
    let init = |id: i32| crate::rerun::params::producer(gx, id) == Some(KindId::NestInitplan) && !rearmed(id);
    if exprs.iter().any(|&e| reads(e, &init)) {
        shape(node, ROWC_INITPLAN);
    }
    true
}

/// D2739 NEST-INNER (modifytable, subq-0323: `WITH d AS (DELETE FROM t WHERE k IN (SELECT id FROM o WHERE ..) RETURNING
/// ..)`): a data-modifying CTE's DELETE whose pipeline `k` scans a REFERENCE-marked relation o (its own heap scan under
/// non-VOLATILE filters, gates and projections) and joins it by one INNER nest op to its inner, the target's own heap
/// range / index scan under filters and projections, then only filters, gates and projections up to the write op `j`,
/// the write marking only o. EvalPlanQual refetches o's row at its junk ctid (the version the join read, so its quals
/// pass again) and reruns the inner over the target's newest version and the nest's join quals (epq_ir.rs
/// `epq_of_inner`). The expressions the recheck evaluates and `(nest op, inner pipeline)`, else none. D3602 (River's
/// JobRetry: `updated_job AS (UPDATE river_job SET .. FROM job_to_update WHERE river_job.id = job_to_update.id ..)`, a
/// Nested Loop whose outer is the CTE Scan of the locking CTE job_to_update, its inner the job table's index scan;
/// D3507's graphile-worker get_job `q` too): an UPDATE too, and the outer a breaker's rows under one COPY mark (the
/// CTE's store), whose stored row EvalPlanQual substitutes, the row the nest holds; nothing is refetched. D3686
/// (pg-boss's id lists over few rows): or the target's scan under a Memoize and its pass-through projection
/// (ref_nest.rs `memo_tail`): the redo stops before the Memoize, as EvalPlanQual's fresh one caches nothing and runs
/// the scan. D3601 (7) (River's completer's updated_metadata_only over its id list): or, for a data-modifying CTE's
/// write, the inner's hashed SubPlan steps at the row, their sets built once (`inner_sets`), as `probed` takes them.
/// D3750 (graphile-worker 0.18's complete_job_q: `WITH j AS (DELETE FROM _private_jobs AS jobs USING unnest($1::bigint[])
/// n(n) WHERE id = n RETURNING *) UPDATE _private_job_queues .. FROM j ..`, a Nested Loop whose outer is the Function Scan
/// of unnest, its inner the job table's index scan parameterized by the id): or the outer a Function Scan (`from_step`)
/// under the one COPY mark, its FROM step the row's own op below the copy entry, never run again. D4662 (DVSTATS' key
/// form: `WITH d AS (DELETE FROM t USING (VALUES (5), (15)) v(x) WHERE id = v.x ..)`, a Nested Loop whose outer is the
/// Values Scan and whose inner the target's primary key index scan by the key): or the outer a VALUES list's own scan.
pub(super) fn nest_inner<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, (usize, usize))> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::plannodes::RowMarkType;
    // D3602: an UPDATE too (River's JobRetry / JobCancel)
    // D3617: or one pruned leaf of a partitioned root (`pruned_leaf`)
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.targets.len() != 1 || w.route.is_some() && !pruned_leaf(w) || w.members.has(Member::TargetView) {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    // the outer: a REFERENCE-marked relation's own heap scan, or (D3602) a breaker's rows (a CTE's store) under a COPY
    // mark
    let outer = match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } => Some((u32::from(*scanrelid), *relid)),
        Source::Breaker { .. } => None,
        // D3750 (graphile-worker's complete_job_q): or, in a data-modifying CTE, a Function Scan's FROM step over the constant
        // source (`from_step`), its row a COPY mark's
        Source::Const if from_step(p) && gx.cte_write(k) => None,
        // D4662: or a VALUES list's own scan, its row a COPY mark's
        Source::Values { .. } if gx.cte_write(k) => None,
        _ => return None,
    };
    // D3795: past nests over a merge or hash join's Material (`held_nest`)
    let mut nests = p.ops[..j].iter().enumerate().filter(|(i, o)| matches!(o, Op::Join(_)) && !held_at(gx, k, *i));
    let (Some((jn, Op::Join(JoinDesc::Nest { jointype: types_nodes::jointype::JoinType::JOIN_INNER, qual, .. }))), None) = (nests.next(), nests.next()) else { return None };
    let ik = ps.inner_of(k, jn)?;
    let q = &ps[ik];
    let own = match &q.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } => *relid == w.plan.target_rel && u32::from(*scanrelid) == w.rti,
        _ => false,
    };
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) | Op::Gate(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        _ => false,
    };
    // D3621: or the first-of-run de-duplicating the Sort the pipeline reads (`deduped`'s sorted form): the recheck's
    // one row is a run of its own, and the redo holds the row the nest holds
    let run = |o: &Op<'_>| matches!(o, Op::FirstOfRun(_)) && outer.is_none() && sorted_run(gx, k);
    // the COPY row's own ops below its entry (merge/admit.rs `own_entry`: the Function Scan's FROM step), which a recheck
    // never runs: EvalPlanQual hands the scan its stored row and never calls the function again
    let entry = if outer.is_none() { crate::ops::modifytable::merge::admit::own_entry(gx, k, w) } else { 0 };
    let row_own = |i: usize, o: &Op<'_>| i < entry && crate::ops::modifytable::merge::admit::subquery_own(o);
    if !own || outer.is_some_and(|(_, orel)| orel == w.plan.target_rel) || !p.ops[..jn].iter().enumerate().all(|(i, o)| quiet(o) || run(o) || row_own(i, o) || held_at(gx, k, i)) {
        return None;
    }
    // D3686: or a Memoize (and its pass-through projection) over the inner's scan: the redo stops before it
    // (epq_ir.rs `epq_of_inner`), as EvalPlanQualStart's fresh Memoize caches nothing and runs the scan
    let ji = crate::rerun::epq::ref_nest::inner_end(q);
    if !crate::rerun::epq::ref_nest::memo_tail(q) {
        return None;
    }
    // D3601 (7) (River's completer's updated_metadata_only over its id list: the target's index scan the nest's inner,
    // its filter a hashed NOT IN over updated_running's store): or the inner's hashed SubPlan steps (`inner_sets`),
    // which the redo probes as built
    let (mut out, ids) = match below_in(gx.mcx, q, ji) {
        Some(out) => (out, PgVec::new_in(gx.mcx)),
        None => inner_sets(gx, k, ik, w)?,
    };
    // D3795: each held nest's sides, as `probed` counts them; its row is in the one the target's nest holds
    let (mut rels, mut copies): (PgVec<'_, (u32, u32)>, usize) = (PgVec::new_in(gx.mcx), 0usize);
    for i in (0..jn).filter(|&i| held_at(gx, k, i)) {
        held_exprs(gx, k, i, (&mut rels, &mut copies), &mut out)?;
    }
    out.extend_in(qual.iter());
    for o in p.ops[jn + 1..j].iter() {
        match o {
            Op::Filter(x) | Op::Gate(x) => out.extend_in(conjuncts(*x)),
            Op::Project(l) => out.extend_in(l.iter()),
            _ => return None,
        }
    }
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == x));
    let others: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    // D3602: or the one COPY mark whose stored row the breaker hands on, its copy entry this pipeline's op at or before
    // the nest (marks.rs `own_copy_at`: copy_row.rs `boundary`, D4662 the VALUES scan's 0): EvalPlanQual substitutes that
    // row, the one the nest holds
    let copied = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::marks::own_copy_at(gx, k, m.rti).is_some_and(|at| at <= jn);
    let first = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| match outer {
        Some((orti, orel)) => m.mark == RowMarkType::ROW_MARK_REFERENCE && m.rti == orti && m.relid == orel && m.ctid_col > 0,
        None => copied(m) || deduped(gx, k, m),
    };
    // D3795: every other mark a held nest's side's: a REFERENCE mark each relation's, a COPY mark each breaker's (the
    // redo reads the held row, never a stored row or a refetch)
    let rest: PgVec<'_, _> = others.iter().copied().filter(|m| !first(m)).collect_in(gx.mcx);
    let reference = |m: &&&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_REFERENCE && m.ctid_col > 0 && rels.iter().any(|r: &(u32, u32)| r.0 == m.rti && r.1 == m.relid);
    let copy = |m: &&&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && m.ctid_col > 0;
    let (nref, ncopy) = (rest.iter().filter(|m| reference(m)).count(), rest.iter().filter(|m| copy(m)).count());
    let held = nref == rels.len() && ncopy == copies && rest.len() == nref + ncopy;
    // D3800: past a held drain, whose build's rows are a side too, no mark is the outer's own
    let drained = (0..jn).any(|i| held_drain(gx, k, i).is_some());
    let marks = others.len() == rest.len() + usize::from(!drained) && held && w.mark_descs.iter().all(|m| !m.parent);
    (marks && !rerun(k) && !out.iter().any(|&e| gravity_decompose::expr::has_subplan(e) && !stepped(e, &ids))).then_some((out, (jn, ik)))
}

/// D3601 (7): nest inner `ik` of data-modifying CTE write pipeline `k` (write `w`, no SubPlan in its own expressions)
/// runs, up to its Memoize, filters, gates, projections and hashed SubPlan steps at the row, at least one step
/// (`below_sets`), their sets built once (`cte_built`), a set whose plan reads as acting only over quiet pipelines
/// (`plain_set`), as `probed` takes them: the expressions and the steps' SubPlan ids, else none. EvalPlanQualStart's
/// fresh SubPlan states build each set again from the same rows; the redo probes the set as built (epq_ir.rs
/// `epq_of_inner`, `Redo::Sub`). nest_write.rs `dml_form` asks it too, so the write's form and its recheck agree.
pub(crate) fn inner_sets<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, ik: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, PgVec<'g, i32>)> {
    if !gx.cte_write(k) || w.members.has(Member::WriteSubPlan) {
        return None;
    }
    let q = &gx.pipelines()[ik];
    let (out, sets) = below_sets(gx.mcx, q, crate::rerun::epq::ref_nest::inner_end(q))?;
    let slots: PgVec<'_, u8> = sets.iter().map(|x| x.0).collect_in(gx.mcx);
    let built = cte_built(gx, k, &slots);
    let unlicensed = sets.iter().any(|x| !x.1);
    if built.is_empty() || unlicensed && !built.iter().all(|&b| plain_set(gx, b)) {
        return None;
    }
    Some((out, sets.iter().map(|x| x.2).collect_in(gx.mcx)))
}

/// D3601 (7): `below` for a nest inner `q` whose ops before `ji` are filters, gates, projections and hashed SubPlan
/// steps at the row (`set_step`), at least one: the expressions (each step's args and test among them) and, per step,
/// the slot of its set, whether its licence for fewer runs holds and its SubPlan id; else none.
fn below_sets<'g, 'p>(mcx: Mcx<'g>, q: &PipelineNode<'p>, ji: usize) -> Option<(PgVec<'g, Node<'p>>, PgVec<'g, (u8, bool, i32)>)> {
    let mut out: PgVec<'g, Node<'p>> = match &q.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().collect_in(mcx),
        _ => PgVec::new_in(mcx),
    };
    let mut sets: PgVec<'g, (u8, bool, i32)> = PgVec::new_in(mcx);
    for o in q.ops[..ji].iter() {
        match o {
            Op::Filter(x) | Op::Gate(x) => out.extend_in(conjuncts(*x)),
            Op::Project(l) => out.extend_in(l.iter()),
            Op::SubPlan(d) => {
                let (slot, licensed) = set_step(d)?;
                sets.push_in((slot, licensed, d.plan_id));
                out.extend_in(d.args.iter().chain(d.test));
            }
            _ => return None,
        }
    }
    (!sets.is_empty()).then_some((out, sets))
}

/// D3650 NEST-COPY (graphile-worker's get_job over one named queue: `q AS (UPDATE job_queues SET .. FROM j WHERE
/// job_queues.id = j.job_queue_id)`, a Nested Loop whose outer is the target's scan and whose inner is the CTE Scan of
/// the claim j, its Join Filter the key): a data-modifying CTE's UPDATE / DELETE of one target whose pipeline `k` is
/// the target's own heap scan under non-VOLATILE filters, gates and projections, joined by one unparameterized INNER
/// nest op to its inner, a CTE's stored rows (a breaker source) under non-VOLATILE filters and projections whose COPY
/// mark is the write's one other mark, its stored row entering in that inner (marks.rs `own_copy_at`), then only such
/// filters, gates and projections up to the write op `j`. EvalPlanQual hands the CTE Scan the stored row, the one the
/// nest handed on, and reruns the target's scan over the newest version and the nest's join quals; the redo does that
/// over the inner row the nest holds (epq_ir.rs `Redo::Nest`, `epq_of_subs`), nothing scanned or run again. Neither
/// pipeline in a rerun set. The expressions the recheck evaluates (the inner's too), else none. D3751 (graphile-worker
/// 0.18's fail_job_q: `WITH j AS (UPDATE _private_jobs AS jobs SET .. FROM json_array_elements($2::json) AS els(el) WHERE
/// id = (el->>'jobId')::bigint AND locked_by = $1 RETURNING *) ..`, a Nested Loop whose outer is the target's scan and whose
/// inner is the Function Scan, its Join Filter the key): or the inner a Function Scan (`from_step`) under the COPY mark,
/// its stored row entering past the FROM step: EvalPlanQual hands the scan that row and never calls the function again.
/// D4662 (DVSTATS' `WITH d AS (DELETE FROM t USING (VALUES (1), (1)) v(x) WHERE .. RETURNING id) SELECT count(*) FROM d`,
/// a Nested Loop whose outer is the target's scan and whose inner is the Values Scan, no join qual or the key a Join
/// Filter): or the inner a VALUES list's own scan under the COPY mark: EvalPlanQual hands it the stored row and never
/// evaluates the list again.
fn copy_outer<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, usize)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::plannodes::RowMarkType;
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.targets.len() != 1 || w.route.is_some() || w.members.has(Member::TargetView) {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let own = match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => *relid == w.plan.target_rel && u32::from(*scanrelid) == w.rti,
        _ => false,
    };
    let mut nests = p.ops[..j].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_)));
    let (Some((jn, Op::Join(JoinDesc::Nest { jointype: types_nodes::jointype::JoinType::JOIN_INNER, params, qual, .. }))), None) = (nests.next(), nests.next()) else { return None };
    let ik = ps.inner_of(k, jn)?;
    let q = &ps[ik];
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) | Op::Gate(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        _ => false,
    };
    let plain = p.ops[..j].iter().enumerate().all(|(i, o)| i == jn || quiet(o));
    // D3751 (graphile-worker's fail_job_q): or a Function Scan's FROM step over the constant source (`from_step`), its
    // ops past the step quiet; D4662: or a VALUES list's own scan, its row entering at the source (marks.rs `own_copy_at`)
    let body = if from_step(q) { &q.ops[1..] } else { &q.ops[..] };
    let inner = (matches!(q.source, Source::Breaker { .. } | Source::Values { .. }) || from_step(q)) && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) && quiet(o));
    if !own || !params.is_nil() || !plain || !inner || qual.iter().any(|x| crate::effects::volatile(gx, x)) {
        return None;
    }
    // the write's one other mark: the CTE's COPY mark, its stored row entering in the inner
    let others: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    let copied = others.len() == 1 && w.mark_descs.iter().all(|m| !m.parent) && others[0].mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::marks::own_copy_at(gx, ik, others[0].rti).is_some();
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == x));
    if !copied || rerun(k) || rerun(ik) {
        return None;
    }
    let mut out = below_ops(gx.mcx, p, j, jn, 0)?;
    out.extend_in(qual.iter());
    for o in body.iter() {
        match o {
            Op::Filter(x) => match x.as_list() { Some(l) => out.extend_in(l.iter()), None => out.push_in(*x) },
            Op::Project(l) => out.extend_in(l.iter()),
            _ => return None,
        }
    }
    (!out.iter().any(|&e| gravity_decompose::expr::has_subplan(e))).then_some((out, jn))
}

/// D3650 NEST-COPY: data-modifying CTE pipeline `k`'s copy entry when its write rechecks in its IR by `copy_outer`:
/// `(k, op past its nest)`, the nest's inner reading the COPY-marked store (rerun/epq/marks.rs `beside_ir_entries`).
pub(crate) fn copy_outer_entry(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, usize)> {
    let p = &gx.pipelines()[k];
    let (j, w) = p.ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None })?;
    if !cte_in_ir(gx, k) {
        return None;
    }
    copy_outer(gx, k, j, w).map(|x| (k, x.1 + 1))
}

/// A Function Scan's pipeline: the constant source, its first op the function's FROM step (decompose srf.rs
/// `SrfPlace::From`). As the target's nest outer (`nest_inner`) or inner (`copy_outer`) its row is the write's COPY mark's,
/// which EvalPlanQual hands the scan (EvalPlanQualFetchRowMark's copy leg): the function is never called again, and the
/// redo works over the row the nest holds.
fn from_step(p: &PipelineNode<'_>) -> bool {
    use gravity_decompose::ops::shared::srf::SrfPlace;
    matches!(p.source, Source::Const) && matches!(p.ops.first(), Some(Op::Srf(d)) if matches!(d.place, SrfPlace::From { .. }))
}

/// D3647 BUILT-COPY's answer (`built_copy`): the build's pipeline and its build op, the probe op and the op of `k` where
/// the COPY mark's stored row enters (copy_row.rs `boundary`).
pub(crate) struct BuiltCopy {
    pub build: (usize, usize),
    pub probe: usize,
    pub entry: usize,
}

/// D3647 BUILT-COPY (graphile-worker 0.17+ / 0.18.0's batched get_jobs over fewer queues than the batch: `q AS (UPDATE
/// _private_job_queues AS job_queues .. FROM j WHERE job_queues.id = j.job_queue_id)` as a Hash Join whose build is the
/// target's own scan and whose probe is the CTE Scan of j): a data-modifying CTE's UPDATE of one target (RETURNING or none),
/// whose pipeline `k` reads a CTE's stored rows (a breaker source) whose COPY mark is the write's one other mark, its
/// stored row entering at `k`'s op `entry` (copy_row.rs `boundary`), then non-VOLATILE filters, gates and projections, ONE
/// INNER hash probe whose build is the target's own heap scan (its junk ctid the write's position: update_built.rs) under
/// non-VOLATILE filters and projections up to its build op, its keys non-VOLATILE too, then such filters, gates and
/// projections up to the write op `j`. EvalPlanQual hands the CTE Scan its stored row, reruns the target's scan over the
/// newest version (the Hash's one row) and the probe's hash clauses and join qual over the pair; the redo does that
/// (epq_ir.rs `epq_of_build_copy`: REFBUILD's `Redo::Outer` over the stored record, D3569's copy leg), nothing scanned or
/// run again. Neither pipeline in a rerun set; no SubPlan; a probe whose clauses run on each candidate (D1899) keeps
/// the runner's recheck. The expressions the recheck evaluates and the form's ops, else none.
pub(crate) fn built_copy<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, BuiltCopy)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::plannodes::RowMarkType;
    if w.cmd != Cmd::Update || w.targets.len() != 1 || w.route.is_some() || w.members.has(Member::TargetView) || !gx.cte_write(k) {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    // D3751 (graphile-worker's fail_job_q over few locked jobs): or a Function Scan's rows (`from_step`), its FROM step the
    // row's own op below the copy entry
    let from = from_step(p);
    if !matches!(p.source, Source::Breaker { .. }) && !from {
        return None;
    }
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) | Op::Gate(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let mut joins = p.ops[..j].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_)));
    let (Some((jp, Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, slot, keys, clauses, qual, .. }))), None) = (joins.next(), joins.next()) else { return None };
    if !p.ops[..j].iter().enumerate().all(|(i, o)| i == jp || i == 0 && from || quiet(o)) || keys.iter().chain(clauses.iter()).chain(qual.iter()).any(vol) || !crate::rerun::epq::epq_ir::images_decide(gx, clauses) {
        return None;
    }
    // the build: the target's own heap scan, only quiet filters and projections up to its build op
    let q = super::nest_outer::producer(gx, k, *slot)?;
    let (rti, relid) = super::nest_outer::heap(&ps[q].source)?;
    let (Some(Op::Join(JoinDesc::Build { keys: bkeys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let plain = rest.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) && quiet(o));
    if relid != w.plan.target_rel || rti != w.rti || !plain || bkeys.iter().any(vol) || writes_or_locks(gx, q) {
        return None;
    }
    // the write's one other mark: the CTE's COPY mark, its stored row entering `k`
    let others: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    let ([m], true) = (others.as_slice(), w.mark_descs.iter().all(|m| !m.parent)) else { return None };
    let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let entry = rec.and_then(|r| crate::rerun::epq::copy_row::boundary(gx, k, r)).filter(|&at| at <= jp && (!from || at >= 1))?;
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&n| usize::from(n) == x));
    if m.mark != RowMarkType::ROW_MARK_COPY || m.ctid_col <= 0 || rerun(k) || rerun(q) {
        return None;
    }
    let mut out = below_in(gx.mcx, &ps[q], rest.len())?;
    out.extend_in(bkeys.iter().chain(clauses.iter()).chain(qual.iter()));
    out.extend_in(below_ops(gx.mcx, p, j, jp, usize::from(from))?);
    (!out.iter().any(|&e| gravity_decompose::expr::has_subplan(e))).then_some((out, BuiltCopy { build: (q, rest.len()), probe: jp, entry }))
}

/// `below` in `mcx`.
pub(super) fn below_in<'g, 'p>(mcx: Mcx<'g>, p: &PipelineNode<'p>, j: usize) -> Option<PgVec<'g, Node<'p>>> {
    below_ops(mcx, p, j, j, 0)
}

/// D3650 NEST-COPY (`copy_outer`): `below` for pipeline `p`'s ops below its write op `j` but the nest op `jn`; D3751: and
/// but the ops before `start` (a Function Scan's FROM step, the copied row's own).
fn below_ops<'g, 'p>(mcx: Mcx<'g>, p: &PipelineNode<'p>, j: usize, jn: usize, start: usize) -> Option<PgVec<'g, Node<'p>>> {
    let mut out: PgVec<'g, Node<'p>> = match &p.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().collect_in(mcx),
        _ => PgVec::new_in(mcx),
    };
    for (i, o) in p.ops[..j].iter().enumerate() {
        match o {
            _ if i == jn || i < start => {}
            Op::Filter(q) | Op::Gate(q) => match q.as_list() { Some(l) => out.extend_in(l.iter()), None => out.push_in(*q) },
            Op::Project(l) => out.extend_in(l.iter()),
            _ => return None,
        }
    }
    Some(out)
}

/// D3824 DRAIN-COPY's answer (`drain_copy`): the build's pipeline and its build op, the probe's pipeline, the op where the
/// COPY mark's stored row enters it (copy_row.rs `boundary`) and its probe op (its last), and the drain's Drain op.
pub(crate) struct DrainCopy {
    pub build: (usize, usize),
    pub probe: (usize, usize, usize),
    pub drain: usize,
}

/// D3824 DRAIN-COPY (Hibernate ORM 7's CTE mutation strategy: `with id_cte (id) as materialized (..), delete_cte_X (id) as
/// (delete from X where k in (select id from id_cte) returning k) .. select count(*) from id_cte`, the bulk DELETE /
/// UPDATE of a joined or union-subclass hierarchy, PostgreSQL's Hash Right Semi Join with the target hashed when it is the
/// smaller side, the CTE Scan of id_cte probing it): a data-modifying CTE's UPDATE / DELETE of one target with RETURNING
/// whose rows are a RIGHT SEMI drain's build rows (drain/lower.rs `drained_build_target`: the build the target's own heap
/// scan; the probe hands each build row on at its first match with the probe row, `semi_hands`), the probe side entering
/// the probe pipeline with a CTE's or subquery's stored row whose COPY mark is the write's one other mark (copy_row.rs
/// `boundary`), then non-VOLATILE filters, gates and projections up to the probe; non-VOLATILE ones in the build up to its
/// build op and between the Drain op and the write; keys, clauses and qual non-VOLATILE too. EvalPlanQual hands the CTE
/// Scan its stored row, reruns the target's scan over the newest version (the Hash's one row) and the Hash Right Semi
/// Join's hash clauses and join qual over the pair, which hands the newest version on when they pass; the redo does that
/// (epq_ir.rs `epq_of_drain_copy`: BUILT-COPY's `Redo::Outer` over the stored record, the probe's ops in its own
/// pipeline, the drain pipeline's after the Drain op), nothing scanned or run again. No pipeline of the three in a
/// rerun set; no SubPlan; a probe whose clauses run on each candidate (D1899) keeps the refusal (marks.rs EMIT_ONLY).
/// The expressions the recheck evaluates and the form's ops, else none.
pub(crate) fn drain_copy<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, DrainCopy)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    use types_nodes::plannodes::RowMarkType;
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.returning.is_none() || w.targets.len() != 1 || w.route.is_some() || w.members.has(Member::TargetView) || !gx.cte_write(k) {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let jd = p.ops[..j].iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. })))?;
    let Op::Join(JoinDesc::Drain { matches, .. }) = &p.ops[jd] else { return None };
    let b = gx.drained_build_target(k)?;
    let pr = gx.life_of(k, *matches).map(|l| usize::from(l.0)).filter(|&q| q < k)?;
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) | Op::Gate(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    // the probe: its last op, the RIGHT SEMI probe, its keys, clauses and qual non-VOLATILE, the images its test
    let (Some((Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT_SEMI, keys, clauses, qual, .. }), _)), true) = (ps[pr].ops.split_last(), p.ops[..jd].iter().all(quiet)) else { return None };
    let jp = ps[pr].ops.len() - 1;
    if keys.iter().chain(clauses.iter()).chain(qual.iter()).any(vol) || !crate::rerun::epq::epq_ir::images_decide(gx, clauses) || !p.ops[jd + 1..j].iter().all(quiet) {
        return None;
    }
    // the build: the target's own heap scan (`drained_build_target`'s), only quiet filters and projections up to its build op
    let (Some(Op::Join(JoinDesc::Build { keys: bkeys, .. })), rest) = ps[b].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    if !rest.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) && quiet(o)) || bkeys.iter().any(vol) || writes_or_locks(gx, b) || writes_or_locks(gx, pr) {
        return None;
    }
    // the write's one other mark: the COPY mark whose stored row enters the probe pipeline, quiet ops from there
    let others: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    let ([m], true) = (others.as_slice(), w.mark_descs.iter().all(|m| !m.parent)) else { return None };
    let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let at = rec.and_then(|r| crate::rerun::epq::copy_row::boundary(gx, pr, r)).filter(|&at| at <= jp)?;
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&n| usize::from(n) == x));
    if m.mark != RowMarkType::ROW_MARK_COPY || m.ctid_col <= 0 || !ps[pr].ops[at..jp].iter().all(quiet) || [k, b, pr].into_iter().any(rerun) {
        return None;
    }
    let mut out = below_in(gx.mcx, &ps[b], rest.len())?;
    out.extend_in(bkeys.iter().chain(keys.iter()).chain(clauses.iter()).chain(qual.iter()));
    out.extend_in(below_ops(gx.mcx, &ps[pr], jp, jp, at)?);
    out.extend_in(below_ops(gx.mcx, p, j, jd, jd)?);
    (!out.iter().any(|&e| gravity_decompose::expr::has_subplan(e))).then_some((out, DrainCopy { build: (b, rest.len()), probe: (pr, at, jp), drain: jd }))
}

/// D3870 BUILT-HELD's answer (`built_held`): the build's pipeline and its build op, and the probe op of `k`.
pub(crate) struct BuiltHeld {
    pub build: (usize, usize),
    pub probe: usize,
}

/// D3870 BUILT-HELD (Hibernate ORM 7.4.12's CTE mutation over a joined subclass, BulkManipulationTest's
/// testDeleteOnJoinedSubclass: `delete_cte_rootclass (id) as (delete from rootclass sm1_2 where sm1_2.id1_ in (select
/// id.id from id_cte id) returning id1_)`, PostgreSQL 18.6's Hash Join whose outer is the HashAggregate de-duplicating the
/// CTE Scan of id_cte and whose inner is the Hash of the target's scan): a data-modifying CTE's UPDATE / DELETE of one
/// target whose pipeline `k` joins ONE INNER hash probe to a build of the target's own heap scan (non-VOLATILE filters
/// and projections up to its build op, its keys too), the probe side's rows a COPY mark's stored rows, the write's one
/// other mark: entering `k` itself (copy_row.rs `boundary`) or the producer of the group `k` reads that de-duplicates
/// them (`deduped`), only non-VOLATILE filters, gates and projections from that entry to the probe and from the probe to
/// the write; no pipeline of the three in a rerun set, none but `k` writing or locking, no SubPlan. EvalPlanQual hands
/// the CTE Scan its stored row and reruns the probe side over it (the de-duplicating HashAggregate makes its one group:
/// the row the join read), the target's scan over the newest version (the Hash's one row) and the Hash Join's clauses
/// and join qual over the pair. The probe side being that same row, the redo forms the newest version through the
/// build's ops and joins it to the probe row the probe holds while it hands the match on (hashjoin walk.rs
/// `Env::redo_outers`, epq_ir.rs `epq_of_built_held`'s `Redo::InnerOf`), then runs `k`'s ops between the probe and the
/// write. D3647 BUILT-COPY's UPDATE (its row entering `k`) keeps its own redo. The expressions the recheck evaluates and
/// the form's ops, else none.
pub(crate) fn built_held<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, BuiltHeld)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::plannodes::RowMarkType;
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.targets.len() != 1 || w.route.is_some() || w.members.has(Member::TargetView) || !gx.cte_write(k) {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) | Op::Gate(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let mut joins = p.ops[..j].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_)));
    let (Some((jp, Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, slot, keys, clauses, qual, .. }))), None) = (joins.next(), joins.next()) else { return None };
    if keys.iter().chain(clauses.iter()).chain(qual.iter()).any(vol) || !crate::rerun::epq::epq_ir::images_decide(gx, clauses) {
        return None;
    }
    // the build: the target's own heap scan, only quiet filters and projections up to its build op
    let q = super::nest_outer::producer(gx, k, *slot)?;
    let (rti, relid) = super::nest_outer::heap(&ps[q].source)?;
    let (Some(Op::Join(JoinDesc::Build { keys: bkeys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let plain = rest.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) && quiet(o));
    if relid != w.plan.target_rel || rti != w.rti || !plain || bkeys.iter().any(vol) || writes_or_locks(gx, q) {
        return None;
    }
    // the write's one other mark: a COPY mark whose stored rows the probe side reads, entering `k` (its ops before the
    // entry the subquery's own) or the producer of the group `k` reads (`deduped`: quiet from its entry)
    let others: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    let ([m], true) = (others.as_slice(), w.mark_descs.iter().all(|m| !m.parent)) else { return None };
    if m.mark != RowMarkType::ROW_MARK_COPY || m.ctid_col <= 0 {
        return None;
    }
    let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let (entry, dedup) = match rec.and_then(|r| crate::rerun::epq::copy_row::boundary(gx, k, r)) {
        Some(at) if at <= jp => (at, false),
        Some(_) => return None,
        None if deduped(gx, k, m) => (0, true),
        None => return None,
    };
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&n| usize::from(n) == x));
    let group = dedup.then(|| match ps[k].source {
        Source::Breaker { slot } => super::nest_outer::producer(gx, k, slot),
        _ => None,
    }).flatten();
    if !p.ops[entry..j].iter().enumerate().all(|(i, o)| i + entry == jp || quiet(o)) || rerun(k) || rerun(q) || group.is_some_and(|f| rerun(f) || writes_or_locks(gx, f)) {
        return None;
    }
    let mut out = below_in(gx.mcx, &ps[q], rest.len())?;
    out.extend_in(bkeys.iter().chain(keys.iter()).chain(clauses.iter()).chain(qual.iter()));
    out.extend_in(below_ops(gx.mcx, p, j, jp, entry)?);
    (!out.iter().any(|&e| gravity_decompose::expr::has_subplan(e))).then_some((out, BuiltHeld { build: (q, rest.len()), probe: jp }))
}

/// D3618 (pg-boss's single-job complete: `UPDATE job .. WHERE name = $1 AND id IN (SELECT UNNEST($2::uuid[])) ..`, a
/// Nested Loop whose outer is the HashAggregate de-duplicating the IN subquery's rows, its inner the target's scan
/// parameterized by the id): COPY mark `m`'s stored row enters the producer of pipeline `k`'s breaker source
/// (copy_row.rs `boundary`), a group with keys and no aggregate but its passengers' any_value cells (the subquery's
/// unique-ification; its other columns, the wholerow among them, the group's first row's), only quiet filters and
/// projections from that entry to the group, writing and locking nothing, in no rerun set. EvalPlanQual hands the
/// Subquery Scan the stored row and reruns the HashAggregate over it alone: one group, the row the nest holds, which
/// the redo joins the target's newest version to (epq_ir.rs `epq_of_inner`). D3621 (pg-boss's batch complete, two or
/// more ids: Unique -> Sort): or the producer's last op the Sort whose runs pipeline `k`'s first-of-run de-duplicates
/// (`sorted_run`): EvalPlanQual sorts the stored row alone, one run, the row the nest holds.
fn deduped(gx: &Gx<'_, '_>, k: usize, m: &gravity_decompose::ops::shared::modifytable::RowMark) -> bool {
    use gravity_abi::breaker::BreakerKind;
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return false };
    let Some(q) = super::nest_outer::producer(gx, k, slot) else { return false };
    let pq = &ps[q];
    use gravity_decompose::ops::shared::agg_class::{Class, Collect};
    // the group's other columns ride any_value cells (decompose agg/mod.rs: a unique-ification's unkeyed passengers,
    // the COPY row's wholerow among them), the first row of the group as nodeAgg keeps it
    let passenger = |a: &gravity_decompose::ops::shared::agg::AggDecl<'_>| matches!(a.class, Class::Collect(Collect::AnyValue)) && a.filter.is_none();
    let bare = |g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>| usize::from(g.producer.0) == q && g.aggs.iter().all(passenger) && !g.keys.is_empty() && g.set.is_none() && g.two_pass.is_none();
    let group = crate::frame::iface::end_breaker(gx, q) == Some(BreakerKind::HashAgg) && gx.graph.groups.iter().any(bare);
    let sorted = !group && sorted_run(gx, k);
    let body = if sorted { &pq.ops[..pq.ops.len() - 1] } else { &pq.ops[..] };
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&x| usize::from(x) == q));
    let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
    let Some(at) = rec.and_then(|r| crate::rerun::epq::copy_row::boundary(gx, q, r)) else { return false };
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        _ => false,
    };
    m.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY && (group || sorted) && !rerun && body.get(at..).is_some_and(|r| r.iter().all(quiet))
}

/// D3621: pipeline `k` reads a Sort (its producer's last op, a Sort breaker) whose runs one first-of-run among its ops
/// before its write de-duplicates: PostgreSQL's Unique over the Sort that unique-ifies a semi join's inner.
fn sorted_run(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return false };
    let Some(q) = super::nest_outer::producer(gx, k, slot) else { return false };
    let runs = ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).filter(|o| matches!(o, Op::FirstOfRun(_))).count();
    crate::frame::iface::end_breaker(gx, q) == Some(BreakerKind::Sort) && matches!(ps[q].ops.last(), Some(Op::Sort(_))) && runs == 1
}

/// D2735 COPY-EMIT (modifytable, core-0274: `DELETE FROM t WHERE c IN (SELECT .. INTERSECT SELECT ..) RETURNING ..`, a
/// Hash Join probing a Hash over the Subquery Scan of a HashSetOp): the statement's own UPDATE / DELETE over its one
/// target's own heap scan whose INNER / SEMI hash probes read a breaker's rows under a COPY mark (rowc-copy's
/// `copy_built`, a subquery's product), every other probe a REFERENCE-marked heap build (`probed`), rechecks in the IR
/// as the data-modifying CTE's rowc-copy does: EvalPlanQual substitutes the subquery row the join read (its wholerow
/// junk column), which the matched record holds (epq_ir.rs `Redo::Probe`), and never runs the subquery's plan again,
/// where the runner's recheck would rerun the build's producers (D652). No SubPlan or InitPlan in the recheck's
/// expressions, no other write in the statement (D3792: a CTE that only reads may be a COPY build's or a held nest's
/// side, River's JobSchedule's top-level form), no BEFORE ROW trigger on the command, trigger facts known; false (the
/// runner's recheck and its refusals) for anything else.
fn copy_in_ir(gx: &Gx<'_, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::join::JoinDesc;
    let p = &gx.pipelines()[k];
    let routed = [Member::Route, Member::Targets, Member::CrossPartition, Member::TargetView].into_iter().any(|m| w.members.has(m));
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.targets.len() != 1 || w.route.is_some() || routed {
        return false;
    }
    let own = match &p.source {
        Source::Range { am, scanrelid, relid, .. } | Source::Index { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } => {
            *am == Am::Heap && *relid == w.plan.target_rel && u32::from(*scanrelid) == w.rti
        }
        _ => false,
    };
    // at least one probe of a breaker's rows (a VALUES list's build keeps the runner's recheck)
    let breaker = |slot: u8| super::nest_outer::built(gx, k, slot).is_none() && copy_built(gx, k, slot).is_some_and(|q| matches!(gx.pipelines()[q].source, Source::Breaker { .. }));
    let copies = p.ops[..j].iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if breaker(*slot)));
    // a data-modifying CTE's write (D3792: a CTE that only reads, River's jobs_with_rownum, is the COPY rows' store)
    let ctes = (0..gx.pipelines().len()).any(|q| q != k && gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Write(_))));
    let Some(exprs) = probed(gx, k, j, w).filter(|_| own && copies && !ctes) else { return false };
    let init = |id: i32| crate::rerun::params::producer(gx, id) == Some(KindId::NestInitplan);
    if exprs.iter().any(|&e| gravity_decompose::expr::has_subplan(e) || reads(e, &init)) {
        return false;
    }
    let t = gx.facts.trig_ext(w.plan.target_rel);
    t.known && t.before_row & cmd_bits(w.cmd) == 0
}

/// D2734 ROWC-UNION (modifytable, gen mt-0354: `WITH d AS (DELETE FROM ph WHERE k = 26 RETURNING id, v) ..`, ph partitioned):
/// decompose streams only the statement's own write into the result relations' Append arms (V4R-214); a data-modifying
/// CTE's DELETE of several result relations reads them out of one settled union (merge/route.rs `union_targets`: one
/// feed per result relation, a heap range or index scan under filters and projections into the union's slot, which
/// pipeline `k` reads first), and PostgreSQL's EvalPlanQual re-runs the row's own relation's feed over its newest
/// version, every other result relation's scan blocked (epq_ir.rs `epq_of_union`). Admitted: such a DELETE whose ops
/// between the union's marker (no pruning) and the write are filters, gates and projections, no SubPlan in them or in
/// any feed, no mark but the targets' own, no cross-partition move. The expressions the recheck evaluates (the feeds'
/// and `k`'s), else none.
/// ROWC-UNION-UPDATE (regress triggers#500: `WITH upd AS (UPDATE parted2_stmt_trig SET a = a) ..`): or such an
/// UPDATE without RETURNING (write/update.rs's union form: the SET cells alone, the row's relation its junk tableoid).
/// EvalPlanQual re-runs the row's own relation's feed over its newest version and the SET over it (ExecUpdate's
/// EvalPlanQual then ExecGetUpdateNewTuple); a row whose new partition key leaves its leaf moves through the root as any
/// routed UPDATE's (hosts write_move.rs, ExecCrossPartitionUpdate), so the move member does not refuse here. lockrows
/// UNION-RET: under RETURNING too (write/update.rs's step reads the row's own cells, update_built.rs `ret_cells`).
fn union_rows<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, Node<'p>>> {
    let moves = w.members.has(Member::CrossPartition) || w.route.as_ref().is_some_and(|r| r.cross_partition);
    let update = w.cmd == Cmd::Update;
    if !(w.cmd == Cmd::Delete && !moves || update) || w.targets.len() < 2 {
        return None;
    }
    let p = &gx.pipelines()[k];
    if !matches!(p.source, Source::Breaker { .. }) {
        return None;
    }
    let feeds = crate::ops::modifytable::merge::route::union_targets(gx, k, w)?;
    // the union's marker first (no pruning: the redo's), then only filters, gates and projections up to the write
    let first = p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    if !first || crate::rerun::union_arms::union_of(p).is_none_or(|(_, prunes)| prunes) {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    for o in p.ops[1..j].iter() {
        match o {
            Op::Filter(q) | Op::Gate(q) => match q.as_list() { Some(l) => out.extend_in(l.iter()), None => out.push_in(*q) },
            Op::Project(l) => out.extend_in(l.iter()),
            _ => return None,
        }
    }
    for &f in feeds.iter() {
        let q = &gx.pipelines()[f];
        out.extend_in(below_in(gx.mcx, q, q.ops.len().saturating_sub(1))?);
    }
    let marks = w.mark_descs.iter().all(|m| !m.parent && w.targets.iter().any(|t| t.rti == m.rti));
    (marks && !out.iter().any(|&e| gravity_decompose::expr::has_subplan(e))).then_some(out)
}
/// D652-H: the pipelines that build the tables `tables` pipeline `k` reads (a hashed SubPlan step's): each table's producer
/// and, to a fixpoint, the producers of the slots those read; empty when a table has no producer or one of them is `k`, is
/// in a rerun set (built again per call, not once) or writes or locks, so the caller keeps the runner's recheck.
fn built_once<'g>(gx: &Gx<'g, '_>, k: usize, tables: &[u8]) -> PgVec<'g, usize> {
    let pipes = gx.pipelines();
    let set = fed(gx, (0..pipes.len()).filter(|&q| pipes[q].desc.produces().is_some_and(|s| tables.contains(&s))).collect_in(gx.mcx));
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == q));
    let each = tables.iter().all(|t| set.iter().any(|&q| pipes[q].desc.produces() == Some(*t)));
    if !each || set.iter().any(|&q| q == k || rerun(q) || writes_or_locks(gx, q)) {
        return PgVec::new_in(gx.mcx);
    }
    set
}

/// D3373 (4) UNION-SUBS (modifytable_d652s2_routed i1: `UPDATE ih SET v = (SELECT count(*) FROM ihs WHERE ihs.d = ih.d) WHERE
/// ih.id % 3 = 0`, ih an inheritance tree): the statement's own UPDATE / DELETE of several result relations whose rows come
/// out of one settled union of their feeds (merge/route.rs `union_targets`), with per-row SubPlan steps between the union
/// and the write. PostgreSQL 18.6 pushes the SET list into each child's scan for a partitioned target only
/// (apply_scanjoin_target_to_paths), so an inheritance tree's SubPlan runs in a Result above the Append; here the step is
/// no arm's (decompose union.rs `copies` copies a hashed step alone) and the union is kept in its Materialize form, which
/// the runner's recheck refuses (D652). EvalPlanQual re-runs the row's own relation's scan over its newest version, every
/// other result relation's scan blocked, then the Result: the SubPlan again with the newest version's arguments. The IR's
/// redo is that (update.rs / delete_built.rs `union_epq`, epq_ir.rs `epq_of_union`, whose `ops_at` evaluates each step
/// again over the row so far: `Redo::Sub`). Admitted: ops between the union's marker (no pruning) and the write that are
/// filters, gates, projections and unconditional per-row SubPlan steps at the row (a hashed step's set is built once and
/// probed: kept out), at least one step; after the write only projections (a RETURNING list with no SubPlan: the redo
/// hands the written row on to it as the first try does); every other pipeline a feed or a member of a step's rerun
/// set, none writing or locking; no InitPlan and no data-modifying CTE; marks only the targets' own; no system column past
/// ctid and tableoid in a feed's scan; no cross-partition move, view or BEFORE ROW trigger on any target, every target's
/// trigger facts known.
fn union_subs(gx: &Gx<'_, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::ops::shared::subplan::{SubForm, SubSite};
    let moves = [Member::CrossPartition, Member::TargetView].into_iter().any(|m| w.members.has(m)) || w.route.is_some_and(|r| r.cross_partition);
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || moves || w.targets.len() < 2 || w.members.has(Member::WriteSubPlan) || w.merge.is_some() {
        return false;
    }
    let p = &gx.pipelines()[k];
    if !matches!(p.source, Source::Breaker { .. }) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_))) {
        return false;
    }
    let Some(feeds) = crate::ops::modifytable::merge::route::union_targets(gx, k, w) else { return false };
    let first = p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    if !first || crate::rerun::union_arms::union_of(p).is_none_or(|(_, prunes)| prunes) {
        return false;
    }
    let mut ids: PgVec<'_, i32> = PgVec::new_in(gx.mcx);
    for o in p.ops[1..j].iter() {
        match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => {}
            Op::SubPlan(d) if !d.conditional && d.site == SubSite::Row && !matches!(d.form, SubForm::Hashed { .. }) => ids.push_in(d.plan_id),
            _ => return false,
        }
    }
    if ids.is_empty() {
        return false;
    }
    let pipes = gx.pipelines();
    let member = |q: usize| gx.graph.reruns.iter().any(|r| ids.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    let others = (0..pipes.len()).all(|q| q == k || feeds.contains(&q) || member(q) && !writes_or_locks(gx, q));
    let ctes = (0..pipes.len()).any(|q| gx.cte_write(q));
    let feeds_ok = feeds.iter().all(|&f| {
        let q = &pipes[f];
        let src = q.node_ops.iter().find(|n| n.first_op == 0).map_or(q.node_id, |n| n.node_id);
        let sys = gx.graph.scan_lanes.iter().any(|l| l.node_id == src && l.names().any(|n| n != "ctid" && n != "tableoid"));
        !sys && !writes_or_locks(gx, f) && below_in(gx.mcx, q, q.ops.len().saturating_sub(1)).is_some_and(|x| !x.iter().any(|&e| gravity_decompose::expr::has_subplan(e)))
    });
    let marks = w.mark_descs.iter().all(|m| !m.parent && w.targets.iter().any(|t| t.rti == m.rti));
    let rels = core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid));
    let trig = rels.map(|r| gx.facts.trig_ext(r)).all(|t| t.known && t.before_row & cmd_bits(w.cmd) == 0);
    others && !ctes && gx.graph.init_plans.is_empty() && feeds_ok && marks && trig
}

/// Pipelines `set` and, to a fixpoint, the producers of the slots they read.
fn fed<'g>(gx: &Gx<'g, '_>, mut set: PgVec<'g, usize>) -> PgVec<'g, usize> {
    let pipes = gx.pipelines();
    let feeds = |set: &[usize], q: usize| pipes[q].desc.produces().is_some_and(|s| set.iter().any(|&m| pipes[m].desc.consumes.contains(&s)));
    while let Some(q) = (0..pipes.len()).find(|&q| !set.contains(&q) && feeds(&set, q)) {
        set.push_in(q);
    }
    set
}

/// Pipeline `q` writes or locks rows.
fn writes_or_locks(gx: &Gx<'_, '_>, q: usize) -> bool {
    gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_)))
}

/// D652-S1 (module doc): whether the emit's write op `j` of pipeline `k` rechecks in the IR over its SubPlan steps.
/// D652-S2: or pipeline `k` is one arm of a write of several result relations (V4R-214: the write streams into each
/// target's arm, over that target's own scan) and the other pipelines are those rerun sets and its sibling arms.
fn subs_in_ir(gx: &Gx<'_, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> bool {
    subs_form(gx, k, j, w).is_some()
}

/// D3678 (module doc, D652-H's VOLATILE half): pipeline `k`'s write rechecks in the IR (`subs_in_ir`) and its redo
/// probes the twins of the hashed sets it reads (rerun/epq/set_lock.rs `twin_set`), not the statement's sets.
pub(crate) fn write_twins(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some((j, w)) = p.ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None }) else { return false };
    p.desc.produces().is_none() && subs_form(gx, k, j, w) == Some(true)
}

/// `subs_in_ir`'s answer: none when the write keeps the runner's recheck, else whether its redo probes twins
/// (`write_twins`).
fn subs_form(gx: &Gx<'_, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<bool> {
    let p = &gx.pipelines()[k];
    let several = w.targets.len() > 1;
    let arm = |o: &Op<'_>| crate::graph::is_arm(o);
    // a cross-partition UPDATE (its move's EvalPlanQual is ExecCrossPartitionUpdate's) and a view stay the runner's; one
    // result relation reached through its root (a pruned partitioned target) too, unless that root is the result relation
    // itself (`own_root`)
    let moves = [Member::CrossPartition, Member::TargetView].into_iter().any(|m| w.members.has(m)) || w.route.is_some_and(|r| r.cross_partition);
    let routed = !several && !own_root(w) && ([Member::Route, Member::Targets].into_iter().any(|m| w.members.has(m)) || w.route.is_some());
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || moves || routed || several && !p.ops[..j].iter().any(arm) {
        return None;
    }
    // only filters, gates, projections and SubPlan steps below the write (no join, aggregate or VALUES source: each step is
    // over the pipeline's row), and an arm's marker; D652-H: the steps' effects, for a kept table's licence
    let mut ids: PgVec<'_, i32> = PgVec::new_in(gx.mcx);
    let mut steps = Effects::NONE;
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => {}
            Op::SubPlan(d) => {
                ids.push_in(d.plan_id);
                steps = steps.and(Effects::of_sub_plan(d));
            }
            o if several && arm(o) => {}
            _ => return None,
        }
    }
    // D3681: no SubPlan step below the write and none after it (`post` below empty) never rechecks here (the last test
    // below); asked before the walks over every pipeline, which each arm of a several-target write made
    if ids.is_empty() && !p.ops[j + 1..].iter().any(|o| matches!(o, Op::SubPlan(_))) {
        return None;
    }
    // every other pipeline re-runs at a call of one of them (its rerun set's member): nothing built once; D652-S2: or it is
    // a sibling arm of the write (another target's rows, each arm's recheck its own); D652-H: or it builds a table the
    // pipeline reads (a hashed step's) once (`built_once`), which the steps' licence keeps; D652-I: or it runs an InitPlan
    // (with its producers): the statement's own write rechecks with the value the statement computed, as EvalPlanQualStart
    // forces the InitPlans the EPQ tree reads in the parent and copies their values (none runs again); no CTE in the
    // statement, as before (a data-modifying CTE's InitPlan below its write runs again in the first recheck's own plan,
    // ROWC_INITPLAN's reason)
    let member = |q: usize| gx.graph.reruns.iter().any(|r| ids.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    let ctes = (0..gx.pipelines().len()).any(|q| gx.cte_write(q));
    let init_pipes = fed(gx, gx.graph.init_plans.iter().map(|i| usize::from(i.pipeline)).collect_in(gx.mcx));
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == q));
    // and the redo evaluates every step again (none conditional and none a check option's host-fed one, so none lazy:
    // epq_ir.rs LAZY_SUB) and none acts
    let cond = w.members.has(Member::WriteSubPlan) || p.ops[..j].iter().any(|o| matches!(o, Op::SubPlan(d) if d.conditional));
    // D2904: and every InitPlan the recheck's plan reads at this level has been forced before any row reaches the write
    // (rerun/epq/inits.rs `write_inits_read`, INIT_READ's rule): EvalPlanQualStart forces a pending one, which the IR's
    // copy would not; else the runner's recheck, which forces it
    // D3842 (Hibernate's XmlHolder UPDATE: an InitPlan and correlated SubPlans in one SET): or no InitPlan of a subquery
    // (each non-CTE one attached at the write's own node, its query level's top: decompose graph.rs `InitPlan::attach`),
    // so none that EvalPlanQualStart re-arms inside a SubPlan's plan, the licence's case
    let write_node = p.node_ops.iter().rev().find(|n| usize::from(n.first_op) <= j).map_or(p.node_id, |n| n.node_id);
    let top = |i: &gravity_decompose::graph::InitPlan<'_>| !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte) && i.attach == write_node;
    // or one attached inside such an InitPlan's plan (a min / max aggregate's under its Result: `(SELECT max(id) FROM
    // t)`), which only that plan reads: PostgreSQL copies the outer one's value and never reads the inner one again
    let inside = |i: &gravity_decompose::graph::InitPlan<'_>| {
        gx.graph.init_plans.iter().filter(|t| top(t)).any(|t| fed(gx, mcx::pg_vec![in gx.mcx; usize::from(t.pipeline)]).iter().any(|&q| gx.pipelines()[q].node_ops.iter().any(|n| n.node_id == i.attach)))
    };
    let top_inits = gx.graph.init_plans.iter().all(|i| matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte) || top(i) || inside(i));
    let kept_inits = !ctes && !cond && (effects::licence(steps, Departure::Fewer).is_ok() || top_inits || crate::rerun::epq::sub_lock::write_inits_forced(gx, k, j)) && init_pipes.iter().all(|&q| q != k && !writes_or_locks(gx, q) && !(rerun(q) && !member(q))) && (crate::rerun::epq::inits::write_inits_read(gx, p, j) || crate::rerun::epq::sub_lock::write_inits_forced(gx, k, j));
    let sibling = |q: usize| several && gx.pipelines()[q].ops.iter().any(arm) && gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Write(_)));
    // writesubplan (D1112): a RETURNING list's SubPlan steps stand after the write, and the redo hands its written row on
    // to them as the first try does; what only they read (a rerun set's members, a hashed set's build, an InitPlan only
    // the list reads) no recheck makes again: PostgreSQL's EvalPlanQual re-runs the plan below ModifyTable (EvalPlanQualStart
    // forces only the InitPlans its plan's extParam names), never the ModifyTable's RETURNING expressions
    let post: PgVec<'_, i32> = p.ops[j + 1..].iter().filter_map(|o| if let Op::SubPlan(d) = o { Some(d.plan_id) } else { None }).collect_in(gx.mcx);
    let init_below = |i: &gravity_decompose::graph::InitPlan<'_>| !ids.is_empty() || below_in(gx.mcx, p, j).is_none_or(|x| x.iter().any(|&e| reads(e, &|id| i.params.iter().any(|q| q == id))));
    let after = |q: usize| {
        let set = gx.graph.reruns.iter().any(|r| post.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
        let build = gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::SubPlan(d) if post.contains(&d.plan_id)));
        set || build || gx.graph.init_plans.iter().any(|i| i.pipeline as usize == q && !init_below(i))
    };
    let inits = gx.graph.init_plans.iter().all(|i| !init_below(i)); // below the write: ROWC_INITPLAN's reason
    // D652-H: the tables the pipeline reads that no rerun member and no post-write step makes, built once, under the steps'
    // licence
    let made_by = |s: u8, f: &dyn Fn(usize) -> bool| (0..gx.pipelines().len()).any(|q| f(q) && gx.pipelines()[q].desc.produces() == Some(s));
    let tables: PgVec<'_, u8> = p.desc.consumes.iter().copied().filter(|&s| !made_by(s, &member) && !made_by(s, &after)).collect_in(gx.mcx);
    let built = built_once(gx, k, &tables);
    // D3678: steps with no licence (a VOLATILE call in a set's plan) keep no table; the redo probes a twin of each set,
    // built again at its first probe, as EvalPlanQualStart's fresh SubPlan states rebuild it (set_lock.rs `twin_set`)
    let twin = !tables.is_empty() && effects::licence(steps, Departure::Fewer).is_err();
    let twins = || tables.iter().all(|&t| built.iter().any(|&q| gx.pipelines()[q].desc.produces() == Some(t) && crate::rerun::epq::set_lock::twin_set(gx, q)));
    if !tables.is_empty() && (built.is_empty() || twin && !twins()) {
        return None;
    }
    // D652-I: InitPlans below the write too when `kept_inits` (writesubplan's `inits`: none below, else ROWC_INITPLAN's reason)
    if ids.is_empty() && post.is_empty() || !(inits || kept_inits) || !(gx.reads_product(k) || crate::rerun::epq::sub_lock::subs_lock(gx, k)) || !(0..gx.pipelines().len()).all(|q| q == k || member(q) || sibling(q) || after(q) || built.contains(&q) || kept_inits && init_pipes.contains(&q)) {
        return None;
    }
    // the write's rows are its target's own scan rows: the one target's, or the arm's own target's (the leaf it scans)
    let own = match &p.source {
        Source::Range { am, scanrelid, relid, .. } | Source::Index { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } => {
            let rti = u32::from(*scanrelid);
            *am == Am::Heap && if several { w.targets.iter().any(|t| t.rti == rti && t.relid == *relid) } else { *relid == w.plan.target_rel && rti == w.rti }
        }
        _ => false,
    };
    let marks = w.mark_descs.iter().all(|m| !m.parent && m.rti == w.rti);
    // epq_of's names (a system column past ctid and tableoid) never reached: the runner's recheck keeps such a write
    let src = p.node_ops.iter().find(|n| n.first_op == 0).map_or(p.node_id, |n| n.node_id);
    let sys = gx.graph.scan_lanes.iter().any(|l| l.node_id == src && l.names().any(|n| n != "ctid" && n != "tableoid"));
    // every target's trigger facts known; a BEFORE ROW trigger of the command only on a write of one result relation
    // (D3708, as nest_outer.rs D3486's): PostgreSQL's EvalPlanQual then runs in GetTupleForTrigger, which locks the newest
    // version before any trigger fires; the host's BEFORE ROW prologue is that lock (hosts trigger_before.rs `lock_old`),
    // a traversed lock answers RECHECK with no trigger run, the redo evaluates the steps again over the newest version and
    // the op runs again, its prologue firing the triggers over those cells
    let one = w.targets.len() <= 1 && w.route.is_none();
    let rels = core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid));
    let trig = rels.map(|r| gx.facts.trig_ext(r)).all(|t| t.known && (one || t.before_row & cmd_bits(w.cmd) == 0));
    (own && marks && !sys && trig).then_some(twin)
}

/// D3617 (pg-boss 10's completeJobs: `WITH results AS (UPDATE pgboss.job SET .. WHERE name = $1 AND id IN (SELECT
/// UNNEST($2::uuid[])) ..)`, the job table LIST-partitioned by queue): one leaf of a partitioned root, the one result
/// relation the planner pruned the root to, whose partition key the SET leaves (no row movement: ExecUpdate never
/// routes it, ExecCrossPartitionUpdate never runs). Its route host hands the newest version in the leaf's own layout
/// (hosts write_route.rs `epq_row`), the relation the recheck reads (`epq_rel`); the retry writes that leaf again, as
/// ExecUpdate / ExecDelete retry on their own result relation. A row a concurrent update moved to another partition
/// raises in the host's lock (heapam_tuple_lock's 'already moved to another partition'), as PostgreSQL's does.
fn pruned_leaf(w: &WriteDesc<'_>) -> bool {
    let leaf = w.route.as_ref().is_some_and(|r| !r.cross_partition && r.root_relid != w.plan.target_rel);
    let one = w.targets.len() == 1 && w.targets[0].relid == w.plan.target_rel;
    leaf && one && w.members.has(Member::Route) && !w.members.has(Member::Targets) && !w.members.has(Member::CrossPartition)
}

/// writesubplan (D652-S1): an inheritance root that is its own one result relation, no partitioned root: relhassubclass
/// outlives the last child (only VACUUM / ANALYZE clear it), so PostgreSQL still expands the root, its rootRelation the
/// parent's RTE and the one result relation the parent's child RTE of the same relation. The write host is the target's
/// own (WriteHost under the root: no leaves), its newest version in the target's layout, the scan the target's heap.
fn own_root(w: &WriteDesc<'_>) -> bool {
    let root = w.route.as_ref().is_some_and(|r| !r.cross_partition && r.root_relid == w.plan.target_rel);
    root && w.targets.len() == 1 && w.targets[0].relid == w.plan.target_rel && !w.members.has(Member::Route) && !w.members.has(Member::CrossPartition)
}

/// D652-S2: the relation whose attributes the redo reads from the write site (epq_ir.rs): an arm's own target, the leaf
/// its source scans (its host's WRITE_EPQ_ROW fills the site in that leaf's layout), else the write's one target.
pub(super) fn epq_rel(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> u32 {
    match &gx.pipelines()[k].source {
        Source::Range { relid, .. } | Source::Index { relid, .. } | Source::Bitmap { relid, .. } if w.targets.len() > 1 => *relid,
        _ => w.plan.target_rel,
    }
}

/// D652-S2: the cells a write that rechecks in the IR declares at least: the widest target's row, which WRITE_EPQ_ROW
/// puts in the site for the redo (a DELETE writes no cell, a routed UPDATE only its SET cells).
pub(super) fn epq_width(w: &WriteDesc<'_>) -> usize {
    w.targets.iter().map(|t| t.site.natts()).max().unwrap_or(0)
}

/// The expressions of pipeline `p`'s ops below its write op `j` and of its source's recheck quals, when those ops are only
/// filters, gates and projections (else none: several sites).
pub(super) fn below<'g, 'p>(p: &PipelineNode<'p>, j: usize, mcx: Mcx<'g>) -> Option<PgVec<'g, Node<'p>>> {
    let mut out: PgVec<'g, Node<'p>> = match &p.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().collect_in(mcx),
        _ => PgVec::new_in(mcx),
    };
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(q) | Op::Gate(q) => out.extend_in(conjuncts(*q)),
            Op::Project(l) => out.extend_in(l.iter()),
            _ => return None,
        }
    }
    Some(out)
}

/// rowc-probe: `below` for a data-modifying CTE's write op `j` of pipeline `k` whose ops there are filters, gates,
/// projections and INNER hash probes, each of one REFERENCE-markable relation's own heap scan built once (nest_outer.rs
/// `built`: quiet, no VOLATILE key, no SubPlan step; its pipeline in no rerun set, neither writing nor locking), no
/// SubPlan in a probe's keys or join qual and no relation probed twice, when the write marks exactly those relations
/// besides its target (each REFERENCE, by its junk ctid): the expressions below the write, each probe's clauses and
/// join qual and its build side's among them. The redo matches the newest version against the record each probe handed
/// on (epq_ir.rs `Redo::Probe`, producerchain D), as PostgreSQL's EvalPlanQual joins it to the REFERENCE row refetched
/// at its junk ctid, the version the build read. D3601 (2): for a data-modifying CTE's write, hashed SubPlan steps at
/// the row too (`set_step`), their sets built once (`cte_built`), which the redo probes as built (epq_ir.rs
/// `Redo::Sub`). D3747 (River's JobSchedule): a LEFT probe too, its redo NULL-extending as the join does (epq_ir.rs
/// `Redo::Probe`'s `left`), over those builds or one merge join's (`merge_built`). Else none.
fn probed<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, Node<'p>>> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let p = &ps[k];
    let mut out: PgVec<'g, Node<'p>> = match &p.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().collect_in(gx.mcx),
        _ => PgVec::new_in(gx.mcx),
    };
    let mut rels: PgVec<'_, (u32, u32)> = PgVec::new_in(gx.mcx);
    let mut copies = 0usize;
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == q));
    let sub = |l: &types_nodes::list::NodeList<'_>| l.iter().any(gravity_decompose::expr::has_subplan);
    let mut sets: PgVec<'_, u8> = PgVec::new_in(gx.mcx);
    let mut unlicensed = false;
    for (i, o) in p.ops[..j].iter().enumerate() {
        match o {
            Op::Filter(q) | Op::Gate(q) => out.extend_in(conjuncts(*q)),
            Op::Project(l) => out.extend_in(l.iter()),
            // D3792 (River's JobSchedule with few due jobs): or an unparameterized LEFT / INNER nest over a plan
            // Material of one merge or hash join (`held_nest`), its redo over the inner row the nest handed on
            // (epq_ir.rs `Redo::Nest`)
            Op::Join(JoinDesc::Nest { .. }) => held_exprs(gx, k, i, (&mut rels, &mut copies), &mut out)?,
            // D3843: or a per-row step over its own inner (`row_sub`), its args and test among the expressions
            Op::SubPlan(d) if gx.cte_write(k) && row_sub(gx, d) => out.extend_in(d.args.iter().chain(d.test)),
            // D3601 (2): or, for a data-modifying CTE's write, a hashed SubPlan step at the row (`set_step`), its args
            // and test among the expressions
            Op::SubPlan(d) if gx.cte_write(k) => {
                let (slot, licensed) = set_step(d)?;
                sets.push_in(slot);
                unlicensed |= !licensed;
                out.extend_in(d.args.iter().chain(d.test));
            }
            // D3747 (River's JobSchedule): a LEFT probe too, its redo NULL-extending as the join does (epq_ir.rs
            // `Redo::Probe`'s `left`); or a build of a COPY-marked breaker's rows merge-joined to a REFERENCE-markable
            // relation's own scan (`merge_built`)
            Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_LEFT, slot, keys, clauses, qual, .. }) if merge_built(gx, k, *slot).is_some() => {
                let Some((q, sides)) = merge_built(gx, k, *slot) else { return None };
                let new = |x: &(u32, u32)| !rels.iter().any(|r| r.0 == x.0);
                if sub(keys) || sub(qual) || q.iter().any(|&x| rerun(x) || writes_or_locks(gx, x)) || !sides.iter().flatten().all(new) || sides[0].zip(sides[1]).is_some_and(|(a, b)| a.0 == b.0) {
                    return None;
                }
                for x in sides {
                    match x {
                        Some(x) => rels.push_in(x),
                        None => copies += 1,
                    }
                }
                out.extend_in(clauses.iter().chain(qual.iter()));
                for &x in q.iter() {
                    if let Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } = &ps[x].source {
                        out.extend_in(recheck.iter());
                    }
                    for o in ps[x].ops.iter() {
                        match o {
                            Op::Filter(e) => out.extend_in(conjuncts(*e)),
                            Op::Project(l) => out.extend_in(l.iter()),
                            Op::Join(JoinDesc::Build { keys, .. }) => out.extend_in(keys.iter()),
                            Op::Join(JoinDesc::Merge { clauses, qual, .. }) => out.extend_in(clauses.iter().chain(qual.iter())),
                            _ => {}
                        }
                    }
                }
            }
            Op::Join(JoinDesc::Probe { jointype: jt @ (types_nodes::jointype::JoinType::JOIN_INNER | types_nodes::jointype::JoinType::JOIN_SEMI | types_nodes::jointype::JoinType::JOIN_LEFT), slot, keys, clauses, qual, .. }) => {
                // a SEMI probe hands on its first match only, which EvalPlanQual's test row is (the COPY mark's stored row):
                // the hash keys alone make every match recheck alike; with a join qual past them (D3379 SEMI-QUAL) the probe
                // hands on its own first match on the keys and the qual, a row PostgreSQL's plan may pick too (its bucket
                // order, batching or a nested loop's inner order decide its own), and the recheck tests that row
                // rowc-copy: or a breaker's rows (a CTE's, a subquery's) under a COPY mark (`copy_built`); D3399 ROWC-JOIN:
                // or a heap scan that itself probes such builds (`join_built`; its probes below)
                // D3488 CTECLAIM: or, for a data-modifying CTE's write, a COPY-marked subquery's build whose plan locks
                // (`claim_built`: its ops from the SubqueryScan on, `from`); D3603: or whose plan holds a LIMIT
                let claim = |s: u8| if gx.cte_write(k) && *jt != types_nodes::jointype::JoinType::JOIN_LEFT { claim_built(gx, k, s, w) } else { None };
                let (q, rel, from) = match super::nest_outer::built(gx, k, *slot) {
                    Some((q, rti, relid)) => (q, Some((rti, relid)), None),
                    None => match copy_built(gx, k, *slot) {
                        Some(q) => (q, None, None),
                        None => match claim(*slot) {
                            Some((q, at)) => (q, None, Some(at)),
                            None => join_built(gx, k, *slot).map(|(q, rti, relid)| (q, Some((rti, relid)), None))?,
                        },
                    },
                };
                if sub(keys) || sub(qual) || rel.is_some_and(|x| rels.iter().any(|r| r.0 == x.0)) || rerun(q) || from.is_none() && writes_or_locks(gx, q) {
                    return None;
                }
                match rel {
                    Some(x) => rels.push_in(x),
                    None => copies += 1,
                }
                out.extend_in(clauses.iter().chain(qual.iter()));
                // the build side is below the write in PostgreSQL's plan too: its quals, projections and keys face the
                // same InitPlan and SubPlan tests (`in_ir`); no SubPlan step there (its rerun set would be another pipeline)
                let bp = &ps[q];
                if let (Source::Index { recheck, .. } | Source::Bitmap { recheck, .. }, None) = (&bp.source, from) {
                    out.extend_in(recheck.iter());
                }
                for (i, o) in bp.ops.iter().enumerate().skip(from.unwrap_or(0)) {
                    match o {
                        Op::Filter(x) => out.extend_in(conjuncts(*x)),
                        Op::Project(l) => out.extend_in(l.iter()),
                        Op::Join(JoinDesc::Build { keys, .. }) => out.extend_in(keys.iter()),
                        // D2735: a set operation's emit first under a COPY build (`copy_built`): the subquery's own plan
                        Op::SetOp(_) if i == 0 && rel.is_none() => {}
                        // D3399 ROWC-JOIN: a heap build's own INNER probe of another relation's own heap scan built once
                        // (`join_built`): its row is in the record the write's probe hands on, as EvalPlanQual's refetch of
                        // that relation's REFERENCE mark returns the version the join read; its clauses, its join qual and
                        // its build side's expressions below the write too
                        // D3379 ROWC-JOINCOPY (modifytable_rowc_copy c3: `DELETE FROM t USING (SELECT b.id FROM b JOIN u ..)
                        // x ..`, u a data-modifying CTE): or a build of a breaker's rows under a COPY mark (`copy_built`): the
                        // CTE Scan's row is in the record too, as EvalPlanQual hands that scan the mark's stored row
                        Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, slot: s2, keys: k2, clauses: c2, qual: q2, .. }) if rel.is_some() => {
                            let (b2, rel2) = match super::nest_outer::built(gx, q, *s2) {
                                Some((b2, rti2, relid2)) => (b2, Some((rti2, relid2))),
                                None => (copy_built(gx, q, *s2)?, None),
                            };
                            if sub(k2) || sub(q2) || rel2.is_some_and(|x| rels.iter().any(|r| r.0 == x.0)) || rerun(b2) || writes_or_locks(gx, b2) {
                                return None;
                            }
                            match rel2 {
                                Some(x) => rels.push_in(x),
                                None => copies += 1,
                            }
                            out.extend_in(c2.iter().chain(q2.iter()));
                            let bp2 = &ps[b2];
                            if let Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } = &bp2.source {
                                out.extend_in(recheck.iter());
                            }
                            for (i2, o2) in bp2.ops.iter().enumerate() {
                                match o2 {
                                    Op::Filter(x) => out.extend_in(conjuncts(*x)),
                                    Op::Project(l) => out.extend_in(l.iter()),
                                    Op::Join(JoinDesc::Build { keys, .. }) => out.extend_in(keys.iter()),
                                    // a set operation's emit first under a COPY build (`copy_built`): the subquery's own plan
                                    Op::SetOp(_) if i2 == 0 && rel2.is_none() => {}
                                    _ => return None,
                                }
                            }
                        }
                        _ => return None,
                    }
                }
            }
            // D4664: or one INNER merge join of the target's own scan and a REFERENCE-markable relation's own heap scan
            // under a Sort (`own_ref_merge`), its redo the newest version against the inner row the merge handed on
            // (epq_ir.rs `Redo::Nest` over the merge clauses and the join qual), as EvalPlanQual reruns the merge over that
            // relation's row refetched at its junk ctid, the version the join read
            Op::Join(JoinDesc::Merge { clauses, qual, .. }) => {
                let (at, q, rel) = own_ref_merge(gx, k, j)?;
                if at != i || sub(clauses) || sub(qual) || rels.iter().any(|r| r.0 == rel.0) {
                    return None;
                }
                rels.push_in(rel);
                out.extend_in(clauses.iter().chain(qual.iter()));
                if let Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } = &ps[q].source {
                    out.extend_in(recheck.iter());
                }
                for o in ps[q].ops.iter() {
                    match o {
                        Op::Filter(x) => out.extend_in(conjuncts(*x)),
                        Op::Project(l) => out.extend_in(l.iter()),
                        Op::Sort(_) => {}
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    // D3601 (2): the hashed steps' sets built once, a CTE's store whole among their inputs (`cte_built`); a step whose
    // plan reads as acting (a data-modifying CTE's store, `set_step`) only over quiet pipelines (`plain_set`)
    let built = if sets.is_empty() { PgVec::new_in(gx.mcx) } else { cte_built(gx, k, &sets) };
    if !sets.is_empty() && (w.members.has(Member::WriteSubPlan) || built.is_empty() || unlicensed && !built.iter().all(|&q| plain_set(gx, q))) {
        return None;
    }
    // every other mark one probed relation's: a REFERENCE mark each heap build's, a COPY mark each breaker build's (one
    // per such build: the redo reads the record, never the stored row, so which COPY mark is which build's is moot)
    use types_nodes::plannodes::RowMarkType;
    let marks: PgVec<'_, _> = w.mark_descs.iter().filter(|m| m.rti != w.rti).collect_in(gx.mcx);
    let reference = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_REFERENCE && m.ctid_col > 0 && rels.iter().any(|r| r.0 == m.rti && r.1 == m.relid);
    let copy = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && m.ctid_col > 0;
    let (nref, ncopy) = (marks.iter().filter(|m| reference(m)).count(), marks.iter().filter(|m| copy(m)).count());
    let plain = w.mark_descs.iter().all(|m| !m.parent);
    (rels.len() + copies > 0 && plain && nref == rels.len() && ncopy == copies && marks.len() == nref + ncopy).then_some(out)
}

/// D3601 (2) (River's completer: `updated_metadata_only AS (UPDATE river_job .. FROM job_to_update WHERE .. AND
/// river_job.id NOT IN (SELECT id FROM updated_running) ..)`, the target's scan filtered by a hashed NOT IN and probing
/// the claim's store): `probed`'s hashed SubPlan step `d`, at the row, never conditional: the slot of its set and
/// whether its plan's licence for fewer runs holds (none acts: a read of a data-modifying CTE's store reads as acting,
/// `plain_set` then asks the set's pipelines), else none. The redo probes the set as built (epq_ir.rs `Redo::Sub`);
/// EvalPlanQualStart's fresh SubPlan state builds it again from the same rows.
fn set_step(d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>) -> Option<(u8, bool)> {
    use gravity_decompose::ops::shared::subplan::{SubForm, SubSite};
    let SubForm::Hashed { slot } = d.form else { return None };
    (d.site == SubSite::Row && !d.conditional).then(|| (slot, effects::licence(Effects::of_sub_plan(d), Departure::Fewer).is_ok()))
}

/// D3601 (2): pipeline `q` of a hashed set (`cte_built`) reads a CTE's store or a relation's heap scan and runs only
/// filters and projections up to the set's build (its last op), nothing VOLATILE: built again from the same rows it is
/// the same set.
fn plain_set(gx: &Gx<'_, '_>, q: usize) -> bool {
    use gravity_decompose::ops::shared::subplan::SubForm;
    let p = &gx.pipelines()[q];
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let source = matches!(p.source, Source::Breaker { .. } | Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, .. } | Source::Bitmap { am: Am::Heap, .. });
    let Some((Op::SubPlan(d), rest)) = p.ops.split_last() else { return false };
    let build = matches!(d.form, SubForm::Build) && !d.args.iter().chain(d.test).any(vol);
    source && build && rest.iter().all(|o| match o {
        Op::Filter(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    })
}

/// D3601 (2): every SubPlan expression `e` holds is one of `ids`, a hashed step `probed` admits, whose value the step
/// computed for the row (none an AlternativeSubPlan).
fn stepped(e: Node<'_>, ids: &[i32]) -> bool {
    let mut ok = true;
    gravity_decompose::expr::walk(e, &mut |n| {
        ok &= n.node_tag() != types_nodes::NodeTag::T_AlternativeSubPlan && n.as_sub_plan().is_none_or(|s| ids.contains(&s.plan_id));
        ok
    });
    ok
}

/// D3601 (2): `built_once` for data-modifying CTE write `k`'s hashed sets: the producer of each table `k` reads (its
/// life, nest_outer.rs `producer`: a later set may reuse the slot) and, to a fixpoint, of each slot those read, stopping
/// at another CTE's own pipeline in no rerun set (a data-modifying one's write too): the set reads that CTE's store
/// whole, PostgreSQL's CTE Scan draining it, and EvalPlanQual's CTE Scan reads the same tuplestore (EvalPlanQualStart
/// copies the CTE's param, its leader), so the set is the one PostgreSQL's recheck builds again. Empty when a slot has
/// no producer or one of them is `k`, is in a rerun set or writes or locks.
fn cte_built<'g>(gx: &Gx<'g, '_>, k: usize, tables: &[u8]) -> PgVec<'g, usize> {
    let pipes = gx.pipelines();
    let rerun = |q: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == q));
    let stop = |q: usize| q != k && gx.cte_write(q) && !rerun(q);
    let mut set: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    let mut todo: PgVec<'_, (usize, u8)> = tables.iter().map(|&t| (k, t)).collect_in(gx.mcx);
    while let Some((r, s)) = todo.pop() {
        let Some(q) = super::nest_outer::producer(gx, r, s) else { return PgVec::new_in(gx.mcx) };
        if stop(q) || set.contains(&q) {
            continue;
        }
        if q == k || rerun(q) || writes_or_locks(gx, q) {
            return PgVec::new_in(gx.mcx);
        }
        set.push_in(q);
        todo.extend_in(pipes[q].desc.consumes.iter().map(|&x| (q, x)));
    }
    set
}

/// D3399 ROWC-JOIN (modifytable_rowc_probe: `UPDATE o .. FROM c, r WHERE c.id = o.cid AND r.id = c.region ..`, the Hash over
/// a Hash Join of c and r): the INNER hash probe of `slot`'s build when it is one REFERENCE-markable relation's own heap scan
/// that itself probes INNER, under filters and projections, none VOLATILE, its probes' clauses and quals and its keys none
/// either (each probe's build `probed` then holds to nest_outer.rs `built`, or D3379 ROWC-JOINCOPY's `copy_built`): (the
/// build's pipeline, the relation's range table index, its oid). Every relation of the join is REFERENCE-marked, or a
/// breaker's rows COPY-marked: EvalPlanQual refetches each row the join read at its junk ctid (or hands the CTE Scan its
/// stored row), so the joined row is the record the write's probe hands on, and the redo matches the newest version
/// against it (epq_ir.rs `Redo::Probe`). The build is the one pipeline before `k` that fills `slot`.
fn join_built(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, u32, u32)> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let q = super::nest_outer::producer(gx, k, slot)?;
    let (rti, relid) = super::nest_outer::heap(&ps[q].source)?;
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let mut probes = 0usize;
    for o in rest.iter() {
        match o {
            Op::Filter(x) if !vol(*x) => {}
            Op::Project(l) if !l.iter().any(vol) => {}
            Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, clauses, qual, .. }) if !clauses.iter().chain(qual.iter()).any(vol) => probes += 1,
            _ => return None,
        }
    }
    (probes > 0 && !keys.iter().any(vol)).then_some((q, rti, relid))
}

/// D3747 (River's JobSchedule: `.. FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ..`, unique_conflicts a Merge
/// Join of a CTE Scan of jobs_with_rownum and river_job's unique index): `slot`'s build when it is one INNER merge join
/// of two sides, each a breaker's rows (a CTE's store, COPY-marked) or one REFERENCE-markable relation's own heap scan,
/// under filters and projections (the inner's a Sort last), none VOLATILE, its keys and the merge's clauses and qual
/// none either: (the build's and the inner's pipelines, each side's relation or none for a breaker). EvalPlanQual hands
/// each CTE Scan its stored row and each relation's scan its row refetched at its junk ctid (the version the join read),
/// so the merge joins them again as the record holds them.
fn merge_built(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<([usize; 2], [Option<(u32, u32)>; 2])> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let q = super::nest_outer::producer(gx, k, slot)?;
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    if keys.iter().any(|x| crate::effects::volatile(gx, x)) {
        return None;
    }
    merged(gx, q, rest)
}

/// `merge_built`'s join (D3792: `held_nest`'s too): pipeline `q`'s ops `rest` (before its build or spool op) are one
/// INNER merge join and filters and projections, none VOLATILE, its clauses and qual none either; each side a breaker's
/// rows (COPY-marked) or one REFERENCE-markable relation's own heap scan under such filters and projections (the
/// inner's a Sort last): (`q` and the inner's pipeline, each side's relation or none for a breaker).
fn merged(gx: &Gx<'_, '_>, q: usize, rest: &[Op<'_>]) -> Option<([usize; 2], [Option<(u32, u32)>; 2])> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let mut merges = rest.iter().filter(|o| !quiet(o));
    let (Some(Op::Join(JoinDesc::Merge { jointype: types_nodes::jointype::JoinType::JOIN_INNER, slot: s2, clauses, qual, .. })), None) = (merges.next(), merges.next()) else { return None };
    let q2 = super::nest_outer::producer(gx, q, *s2)?;
    let side = |x: usize| match ps[x].source {
        Source::Breaker { .. } => Some(None),
        ref s => super::nest_outer::heap(s).map(Some),
    };
    let (Some(a), Some(b)) = (side(q), side(q2)) else { return None };
    let inner = match ps[q2].ops.split_last() {
        Some((Op::Sort(_), body)) => body.iter().all(quiet),
        _ => ps[q2].ops.iter().all(quiet),
    };
    (inner && !clauses.iter().chain(qual.iter()).any(vol)).then_some(([q, q2], [a, b]))
}

/// D4664 (`WITH d AS (DELETE FROM t USING s WHERE id = s.k RETURNING id) ..` planned as a Merge Join of t's primary key
/// index scan and a Sort of s's scan): write pipeline `k`'s ops before its write `j` are one INNER merge join and quiet
/// filters and projections (`merged`), the inner a REFERENCE-markable relation's own heap scan under such filters and
/// projections and its Sort, that pipeline in no rerun set, neither writing nor locking: (the merge's op, the inner's
/// pipeline, its relation).
fn own_ref_merge(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(usize, usize, (u32, u32))> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let Some(([_, q], [_, Some(rel)])) = merged(gx, k, &ps[k].ops[..j]) else { return None };
    let at = ps[k].ops[..j].iter().position(|o| matches!(o, Op::Join(JoinDesc::Merge { .. })))?;
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == q));
    (!rerun && !writes_or_locks(gx, q)).then_some((at, q, rel))
}

/// D4664 (epq_ir.rs `redo_n`): op `i` of pipeline `k` is the merge of `own_ref_merge` under the write, which rechecks in
/// the IR (`probed`).
pub(crate) fn own_merge(gx: &Gx<'_, '_>, k: usize, i: usize) -> bool {
    let Some(j) = gx.pipelines()[k].ops.iter().position(|o| matches!(o, Op::Write(_))) else { return false };
    own_ref_merge(gx, k, j).is_some_and(|(at, _, _)| at == i) && in_ir(gx, k)
}

/// D3792 (update.rs's chain): op `i` of pipeline `k` is a nest over a merge or hash join's Material (`held_nest`) and
/// the write rechecks in the IR (`in_ir`: `probed` takes it), never the runner's recheck (which would read the whole
/// Material: nest_outer.rs, D1933 (c)).
pub(super) fn nest_in_ir(gx: &Gx<'_, '_>, k: usize, i: usize) -> bool {
    held_nest(gx, k, i).is_some() && in_ir(gx, k)
}

/// D3792 (nest_write.rs `dml_form`): op `i` of pipeline `k` is a nest over a merge or hash join's Material
/// (`held_nest`).
pub(super) fn held_at(gx: &Gx<'_, '_>, k: usize, i: usize) -> bool {
    held_join(gx, k, i).is_some()
}

/// D3800 (hashjoin/drain/lower.rs `held_write`): drain pipeline `d`'s write rechecks in the IR through the target's nest
/// past its RIGHT drain held (`held_drain`, `held_inner`).
pub(crate) fn drain_held(gx: &Gx<'_, '_>, d: usize) -> bool {
    let p = &gx.pipelines()[d];
    let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else { return false };
    (0..p.ops.len()).any(|i| held_drain(gx, d, i).is_some()) && held_inner(gx, d, w)
}

/// D3795 (nest_write.rs `dml_form`, River's JobSchedule with few jobs due over a large job table: the job's primary key
/// index scan the inner of a Nested Loop whose outer is the Nested Loop Left Join over the Materialize of the
/// conflicts): pipeline `k`'s write `w` rechecks in the IR through the target's nest (`nest_inner`) past a nest over a
/// merge or hash join's Material (`held_nest`), whose marks are that join's sides' (`nest_inner` counts them), so the
/// runner's recheck, which would place each mark at a site, never runs.
pub(super) fn held_inner(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    let Some(j) = p.ops.iter().position(|o| matches!(o, Op::Write(_))) else { return false };
    (0..j).any(|i| held_at(gx, k, i)) && nest_inner(gx, k, j, w).is_some() && in_ir(gx, k)
}

/// D3795: the statement's own UPDATE / DELETE (not a data-modifying CTE's) of the form `held_inner` names: no other
/// write in the statement, no SubPlan or InitPlan in the recheck's expressions, no BEFORE ROW trigger on the command,
/// trigger facts known (`copy_in_ir`'s terms); false (the runner's recheck and its refusals) for anything else.
fn inner_in_ir(gx: &Gx<'_, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> bool {
    let ps = gx.pipelines();
    let held = (0..j).any(|i| held_at(gx, k, i));
    let ctes = (0..ps.len()).any(|q| q != k && ps[q].ops.iter().any(|o| matches!(o, Op::Write(_))));
    let Some((exprs, _)) = nest_inner(gx, k, j, w).filter(|_| held && !ctes) else { return false };
    let init = |id: i32| crate::rerun::params::producer(gx, id) == Some(KindId::NestInitplan);
    if exprs.iter().any(|&e| gravity_decompose::expr::has_subplan(e) || reads(e, &init)) {
        return false;
    }
    let t = gx.facts.trig_ext(w.plan.target_rel);
    t.known && t.before_row & cmd_bits(w.cmd) == 0
}

/// D3792: `held_nest`'s nest at op `i` of pipeline `k`: its join sides' relations into `rels` (each a range table index
/// not yet there) and its breaker sides into `copies`, the expressions PostgreSQL's recheck would evaluate (its join
/// qual, the inner's and the join's) into `out`; none when it is no such nest, a SubPlan sits in its qual, a pipeline
/// of the join sits in a rerun set other than the nest's own (PostgreSQL's rescan of the inner rewinds the Material) or
/// writes or locks.
fn held_exprs<'p>(gx: &Gx<'_, 'p>, k: usize, i: usize, (rels, copies): (&mut PgVec<'_, (u32, u32)>, &mut usize), out: &mut PgVec<'_, Node<'p>>) -> Option<()> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let p = &ps[k];
    // D3799: or a merge's clauses beside its qual; D3800: a drain's (its probe's clauses are the probe pipeline's)
    let qual: PgVec<'_, Node<'p>> = match p.ops.get(i) {
        Some(Op::Join(JoinDesc::Nest { qual, .. })) => qual.iter().collect_in(gx.mcx),
        Some(Op::Join(JoinDesc::Merge { clauses, qual, .. })) => clauses.iter().chain(qual.iter()).collect_in(gx.mcx),
        Some(Op::Join(JoinDesc::Drain { .. })) => PgVec::new_in(gx.mcx),
        _ => return None,
    };
    let (ik, q, sides) = held_join(gx, k, i)?;
    let node = crate::rerun::inner::op_node(p, i);
    let others = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&m| usize::from(m) == x) && !(r.plan_id == 0 && r.node_id == node));
    let new = |x: &(u32, u32)| !rels.iter().any(|r| r.0 == x.0);
    let sub = qual.iter().any(|&e| gravity_decompose::expr::has_subplan(e));
    let twice = sides.iter().flatten().enumerate().any(|(n, a)| sides.iter().flatten().skip(n + 1).any(|b| a.0 == b.0));
    if sub || q.iter().any(|&x| others(x) || writes_or_locks(gx, x)) || !sides.iter().flatten().all(new) || twice {
        return None;
    }
    for x in sides {
        match x {
            Some(x) => rels.push_in(x),
            None => *copies += 1,
        }
    }
    out.extend_in(qual.iter().copied());
    for &x in ik.iter().chain(q.iter()) {
        if let Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } = &ps[x].source {
            out.extend_in(recheck.iter());
        }
        // D3807: a nest's inner index scan's key values
        if let Source::Index { keys, .. } = &ps[x].source {
            out.extend_in(keys.iter().map(|k| k.value));
        }
        for o in ps[x].ops.iter() {
            match o {
                Op::Filter(e) => out.extend_in(conjuncts(*e)),
                Op::Project(l) => out.extend_in(l.iter()),
                Op::Join(JoinDesc::Merge { clauses, qual, .. }) => out.extend_in(clauses.iter().chain(qual.iter())),
                Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => out.extend_in(keys.iter().chain(clauses.iter()).chain(qual.iter())),
                Op::Join(JoinDesc::Build { keys, .. }) => out.extend_in(keys.iter()),
                // D3807: a parameterized nest's qual and parameter values
                Op::Join(JoinDesc::Nest { qual, params, .. }) => out.extend_in(qual.iter().chain(gravity_decompose::facts::hoist::nest_values(params))),
                _ => {}
            }
        }
    }
    Some(())
}

/// `held_nest`'s join or `held_drain`'s at op `i` of pipeline `k`: (a nest's inner pipeline, the join's pipelines, each
/// side's relation or none for a breaker).
fn held_join<'g>(gx: &Gx<'g, '_>, k: usize, i: usize) -> Option<(Option<usize>, PgVec<'g, usize>, PgVec<'g, Option<(u32, u32)>>)> {
    held_nest(gx, k, i).map(|(ik, q, sides)| (ik, mcx::to_vec_in(&q[..], gx.mcx), mcx::to_vec_in(&sides[..], gx.mcx))).or_else(|| held_drain(gx, k, i).map(|(q, sides)| (None, q, sides)))
}

/// D3800 (River's JobSchedule at 5 jobs a run over River's job table: the job's primary key index scan the inner of a
/// Nested Loop whose outer is a Hash Right Join, the conflicts (a Hash Join of the unique index's bitmap scan and a Hash
/// of jobs_with_rownum) probing a Hash of jobs_with_rownum job): op `i` of pipeline `k` is a RIGHT drain, `k`'s source
/// the probed table (its unmatched records), every op of its probe pipeline a filter, a projection or an INNER hash
/// probe, then the RIGHT probe of that table, none VOLATILE; each build's pipeline such filters and projections, then
/// its build, its keys none VOLATILE; each side a breaker's rows (COPY-marked) or one REFERENCE-markable relation's own
/// heap scan: (the probe's and the builds' pipelines, each side's relation or none for a breaker). EvalPlanQual reruns
/// the Hash Right Join over its test tuples, each CTE Scan's stored row and the probe relation's row refetched at its
/// junk ctid (the version the join read; none under a NULL-extended row), so it makes again the one row the drain
/// handed on, joined or NULL-extended, which the redo joins (epq_ir.rs `Redo::InnerOf`).
fn held_drain<'g>(gx: &Gx<'g, '_>, k: usize, i: usize) -> Option<(PgVec<'g, usize>, PgVec<'g, Option<(u32, u32)>>)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    let ps = gx.pipelines();
    let Some(Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT })) = ps[k].ops.get(i) else { return None };
    let Source::Breaker { slot } = ps[k].source else { return None };
    let q = super::nest_outer::producer(gx, k, *matches)?;
    let b = super::nest_outer::producer(gx, k, slot)?;
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let side = |x: usize| match ps[x].source {
        Source::Breaker { .. } => Some(None),
        ref s => super::nest_outer::heap(s).map(Some),
    };
    // a build's pipeline: quiet ops, then its build, its keys none VOLATILE
    let built = |x: usize| matches!(ps[x].ops.split_last(), Some((Op::Join(JoinDesc::Build { keys, .. }), body)) if body.iter().all(quiet) && !keys.iter().any(vol));
    let (Some((Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT, slot: s, keys, clauses, qual, .. }), body)), true) = (ps[q].ops.split_last(), built(b)) else { return None };
    if *s != slot || super::nest_outer::producer(gx, q, *s) != Some(b) || keys.iter().chain(clauses.iter()).chain(qual.iter()).any(vol) {
        return None;
    }
    let (mut made, mut sides) = (mcx::pg_vec![in gx.mcx; q], mcx::pg_vec![in gx.mcx; side(q)?]);
    for o in body {
        match o {
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, slot, keys, clauses, qual, .. }) if !keys.iter().chain(clauses.iter()).chain(qual.iter()).any(vol) => {
                let qb = super::nest_outer::producer(gx, q, *slot).filter(|&x| built(x))?;
                made.push_in(qb);
                sides.push_in(side(qb)?);
            }
            o if quiet(o) => {}
            _ => return None,
        }
    }
    made.push_in(b);
    sides.push_in(side(b)?);
    Some((made, sides))
}

/// D3792 (River's JobSchedule with few due jobs: `.. FROM jobs_with_rownum job LEFT JOIN unique_conflicts uc ..` as a
/// Nested Loop Left Join over a Materialize of the conflicts, a Merge Join of a CTE Scan of jobs_with_rownum and
/// river_job's unique index, or over a large job table a Hash Join of the index's bitmap scan and a Hash of that CTE
/// Scan): the unparameterized LEFT / INNER nest at op `i` of pipeline `k`, its inner a plan Material's rows under
/// filters and projections, none VOLATILE, the Material made by one pipeline whose ops before its spool are `merged`'s
/// merge join or `hashed`'s hash join: (the inner's pipeline, the join's two pipelines, each side's relation or none
/// for a breaker). EvalPlanQual reruns the Material's child over its test tuples, each CTE Scan's stored row and each
/// relation's row refetched at its junk ctid (the version the join read), so it makes again the one row the nest handed
/// on (none under a NULL-extended row: no test tuple), which the redo joins (epq_ir.rs `Redo::Nest`).
/// D3799 (River's JobSchedule over a large job table: the Merge Left Join of the CTE Scan of jobs_with_rownum and a
/// Materialize of the conflicts' merge join): or a LEFT / INNER merge at op `i` whose inner run is such a Material (its
/// spool in the join's own pipeline), its clauses and qual none VOLATILE: (no inner pipeline, the join's, the sides).
/// EvalPlanQual reruns the merge over the outer's and the Material child's test tuples, as it does the nest.
fn held_nest(gx: &Gx<'_, '_>, k: usize, i: usize) -> Option<(Option<usize>, [usize; 2], [Option<(u32, u32)>; 2])> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    let ps = gx.pipelines();
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let (ik, q) = match ps[k].ops.get(i) {
        Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_INNER, params, .. })) if params.is_nil() => {
            let ik = ps.inner_of(k, i)?;
            let Source::Breaker { slot } = ps[ik].source else { return None };
            if !ps[ik].ops.iter().all(|o| matches!(o, Op::Filter(x) if !vol(*x)) || matches!(o, Op::Project(l) if !l.iter().any(vol))) {
                return None;
            }
            (Some(ik), super::nest_outer::producer(gx, ik, slot)?)
        }
        Some(Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_INNER, slot, clauses, qual, .. })) if !clauses.iter().chain(qual.iter()).any(vol) => {
            (None, super::nest_outer::producer(gx, k, *slot)?)
        }
        _ => return None,
    };
    let Some((Op::Join(JoinDesc::Spool), rest)) = ps[q].ops.split_last() else { return None };
    let (made, sides) = merged(gx, q, rest).or_else(|| hashed(gx, q, rest)).or_else(|| nested(gx, q, rest))?;
    Some((ik, made, sides))
}

/// D3807 (`held_nest`, River's JobSchedule after table bloat: Materialize (Nested Loop (CTE Scan jobs_with_rownum, Index
/// Scan using the unique index keyed by its rows))): pipeline `q`'s ops `rest` (before its spool op) are one INNER
/// parameterized nest and filters and projections, none VOLATILE, its parameters and qual none either; its inner one
/// REFERENCE-markable relation's own heap scan under such filters and projections, its keys and recheck none VOLATILE;
/// `q`'s own rows a breaker's (COPY-marked) or such a scan's: (`q` and the inner's pipeline, each side's relation or none
/// for a breaker). EvalPlanQual reruns the nest over the outer's test tuple, the inner's scan over the relation's row
/// refetched at its junk ctid (its index condition rechecked over that version: the one the join read).
fn nested(gx: &Gx<'_, '_>, q: usize, rest: &[Op<'_>]) -> Option<([usize; 2], [Option<(u32, u32)>; 2])> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let mut nests = rest.iter().enumerate().filter(|(_, o)| !quiet(o));
    let (Some((i, Op::Join(JoinDesc::Nest { jointype: types_nodes::jointype::JoinType::JOIN_INNER, params, qual, .. }))), None) = (nests.next(), nests.next()) else { return None };
    let ik = ps.inner_of(q, i)?;
    let a = match ps[q].source {
        Source::Breaker { .. } => None,
        ref s => Some(super::nest_outer::heap(s)?),
    };
    let b = super::nest_outer::heap(&ps[ik].source)?;
    let src = match &ps[ik].source {
        Source::Index { keys, recheck, .. } => !keys.iter().any(|k| vol(k.value)) && !recheck.iter().any(vol),
        Source::Bitmap { recheck, .. } => !recheck.iter().any(vol),
        _ => true,
    };
    (src && !crate::frame::iface::nest_param_vals(params, &vol) && !qual.iter().any(vol) && ps[ik].ops.iter().all(quiet)).then_some(([q, ik], [a, Some(b)]))
}

/// D3795 (`held_nest`): pipeline `q`'s ops `rest` (before its spool op) are one INNER hash probe and filters and
/// projections, none VOLATILE, its keys, clauses and qual none either; its build's pipeline only such filters and
/// projections then the build, its keys none VOLATILE; each side (`q`'s own rows, the build's) a breaker's rows
/// (COPY-marked) or one REFERENCE-markable relation's own heap scan: (`q` and the build's pipeline, each side's
/// relation or none for a breaker).
fn hashed(gx: &Gx<'_, '_>, q: usize, rest: &[Op<'_>]) -> Option<([usize; 2], [Option<(u32, u32)>; 2])> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let vol = |x: Node<'_>| crate::effects::volatile(gx, x);
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !vol(*x),
        Op::Project(l) => !l.iter().any(vol),
        _ => false,
    };
    let mut probes = rest.iter().filter(|o| !quiet(o));
    let (Some(Op::Join(JoinDesc::Probe { jointype: types_nodes::jointype::JoinType::JOIN_INNER, slot, keys, clauses, qual, .. })), None) = (probes.next(), probes.next()) else { return None };
    let qb = super::nest_outer::producer(gx, q, *slot)?;
    let (Some(Op::Join(JoinDesc::Build { keys: bk, .. })), body) = ps[qb].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let side = |x: usize| match ps[x].source {
        Source::Breaker { .. } => Some(None),
        ref s => super::nest_outer::heap(s).map(Some),
    };
    let (Some(a), Some(b)) = (side(q), side(qb)) else { return None };
    let loud = keys.iter().chain(clauses.iter()).chain(qual.iter()).chain(bk.iter()).any(vol);
    (body.iter().all(quiet) && !loud).then_some(([q, qb], [a, b]))
}

/// D3488 CTECLAIM (lockrows JOBQ's IN form inside a data-modifying CTE: `WITH d AS (DELETE FROM job WHERE id IN (SELECT id
/// FROM job .. LIMIT n FOR UPDATE SKIP LOCKED) RETURNING ..) INSERT INTO done SELECT * FROM d`): the INNER / SEMI hash probe
/// of `slot`'s build when that build is a COPY-marked subquery's rows whose plan locks: the one pipeline before `k` that
/// fills `slot`, the stored row of one of write `w`'s COPY marks entering it at `at` (copy_row.rs `boundary`: the
/// SubqueryScan's first op), a row lock below `at` (the subquery's LockRows, the Program.locks entry lock_init.rs
/// `write_pair` lays beside this write), no write in it, quiet filters and projections from `at` to its build op, its keys
/// quiet too. PostgreSQL's EvalPlanQual hands the SubqueryScan the stored row and never runs the subquery's LockRows
/// again, so the redo matches the newest version against the record the probe handed on, as rowc-copy's (`copy_built`).
/// D3603 (River's JobDeleteBefore: `WITH deleted_jobs AS (DELETE FROM river_job WHERE id IN (SELECT id FROM river_job
/// WHERE .. ORDER BY id LIMIT $4) RETURNING *) SELECT count(*) FROM deleted_jobs`): or a subquery whose plan does not
/// lock, its LIMIT below `at` (copy_built's form takes filters and projections only), which EvalPlanQual never runs
/// again either. Some((the build's pipeline, `at`)), else none.
fn claim_built(gx: &Gx<'_, '_>, k: usize, slot: u8, w: &WriteDesc<'_>) -> Option<(usize, usize)> {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::plannodes::RowMarkType;
    let ps = gx.pipelines();
    let q = super::nest_outer::producer(gx, k, slot)?;
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let rows = w.mark_descs.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY && m.rti != w.rti).filter_map(|m| gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti));
    let at = rows.filter_map(|r| crate::rerun::epq::copy_row::boundary(gx, q, r)).find(|&at| at <= rest.len())?;
    let writes = rest.iter().any(|o| matches!(o, Op::Write(_)));
    let quiet = rest[at..].iter().all(|o| match o {
        Op::Filter(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        _ => false,
    });
    (!writes && quiet && !keys.iter().any(|x| crate::effects::volatile(gx, x))).then_some((q, at))
}

/// D3488 CTECLAIM (rowop.rs `two_plans_served`): pipeline `q`'s row lock is the program's one, the claim a data-modifying
/// CTE's UPDATE / DELETE reads through its COPY mark (`claim_built`), that write rechecking in its own IR and every other
/// write a plain INSERT: the lock is plan.rs `Plan::write_pair`'s Program.locks entry beside the write, rechecked in its
/// own IR (a product-form lock), so neither stops for the runner's EvalPlanQual, which knows one write site.
pub(super) fn claim_lock(gx: &Gx<'_, '_>, q: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let write = |k: usize| ps[k].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let locks = ps.iter().flat_map(|p| p.ops.iter()).filter(|o| matches!(o, Op::Lock(_))).count();
    // D3489 CTECLAIM-INIT: or the lock is in the pipelines of an InitPlan at that write's ModifyTable which its first
    // recheck runs again (rerun/epq/inits.rs `cte_write_armed`)
    let init = |k: usize| {
        let jk = ps[k].ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(ps[k].ops.len());
        crate::rerun::epq::inits::cte_write_armed(gx, k, jk).into_iter().any(|x| gx.with_producers([gx.graph.init_plans[x].pipeline as usize]).contains(&q))
    };
    let mut claims = (0..ps.len()).filter(|&k| gx.cte_write(k)).filter(|&k| {
        write(k).is_some_and(|w| matches!(w.cmd, Cmd::Update | Cmd::Delete) && (ps[k].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if claim_built(gx, k, *slot, w).is_some_and(|(b, _)| b == q))) || init(k)))
    });
    let (Some(k), None) = (claims.next(), claims.next()) else { return false };
    let insert = |w: &WriteDesc<'_>| w.cmd == Cmd::Insert && w.on_conflict.is_none() && w.merge.is_none();
    let others = ps.iter().enumerate().filter(|&(x, _)| x != k).flat_map(|(_, p)| p.ops.iter()).all(|o| !matches!(o, Op::Write(w) if w.cmd != Cmd::Select && !insert(w)));
    locks == 1 && others && in_ir(gx, k)
}

/// rowc-copy: the INNER / SEMI hash probe of `slot`'s build when it is the rows of a breaker (a CTE's store, a subquery's
/// product, D2735 a set operation's emit) or of a VALUES list under filters and projections, none VOLATILE, its keys
/// none either: the build's
/// pipeline, the one before `k` that fills `slot`. The relation those rows are is COPY-marked: PostgreSQL's EvalPlanQual
/// substitutes the row the join read (its `wholerow` junk column), which the matched record holds; the store or the
/// VALUES list itself is never evaluated again, the filters and projections over it are.
fn copy_built(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<usize> {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let q = super::nest_outer::producer(gx, k, slot)?;
    let (Source::Breaker { .. } | Source::Values { .. }) = ps[q].source else { return None };
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    // D2735: a breaker's own emit first (a HashSetOp's: core-0274's INTERSECT) is the subquery's plan, below its scan,
    // which EvalPlanQual never runs again
    let rest = match rest.split_first() {
        Some((Op::SetOp(_), r)) if matches!(ps[q].source, Source::Breaker { .. }) => r,
        _ => rest,
    };
    let quiet = rest.iter().all(|o| match o {
        Op::Filter(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        _ => false,
    });
    (quiet && !keys.iter().any(|x| crate::effects::volatile(gx, x))).then_some(q)
}

/// Whether `x` reads an executor parameter `id` names; a node the walker does not open counts as a read.
pub(crate) fn reads(x: Node<'_>, id: &dyn Fn(i32) -> bool) -> bool {
    crate::stack_check();
    if x.as_param().is_some_and(|r| r.paramkind == ParamKind::PARAM_EXEC && id(r.paramid)) {
        return true;
    }
    let mut got = false;
    !gravity_decompose::expr::kids(x, &mut |c| got = got || reads(c, id)) || got
}

/// The op's status `st`, as the parameter of the block its arms branch from when the op rechecks in the IR (`epq`): the
/// retry's own call comes back to it. Without `epq` the IR is the step's as before.
pub(super) fn arms(b: &mut FunctionBuilder<'_>, st: Value, epq: bool) -> (Value, Option<Block>) {
    if !epq {
        return (st, None);
    }
    let l = b.create_block(&[Type::I64]);
    // the retry's back edge (redo_act) enters here: a declared header, as the backend takes only those as loops (tpde view.rs)
    b.mark_loop_header(l);
    b.br(l, &[st]);
    b.switch_to(l);
    (b.block_params(l)[0], Some(l))
}

/// The RECHECK arm of write site `site`'s op `op` (plan node `node`) when it rechecks in the IR: the newest version into the
/// site (an error stops at the row), the kind's recheck (`rerun`), and on a passing version its cells (`cells`, stored by
/// `store`) and the op again, whose status branches to `l_st`; a failing one joins `l_join` with the carry as it came.
#[allow(clippy::too_many_arguments)]
pub(super) fn retry<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, at: (usize, i32, u32), rerun: &dyn EpqRerun, cells: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> PgVec<'e, (Value, Value)>, store: &dyn Fn(&mut FunctionBuilder<'_>, &Env, &[(Value, Value)]), l_st: Block, l_join: Block, x: &Exits) {
    let (site, node, op) = at;
    let l_epq = b.create_block(&[]);
    b.br(l_epq, &[]);
    b.switch_to(l_epq);
    crate::frame::cancel::check(b, env.ctx, x.l_cancel); // the retry's back edge to the op's arms (redo_act) is a loop
    let st = host_op(b, env, site, WRITE_EPQ_ROW);
    let (dn, l_bad, l_on) = (b.iconst(Type::I64, WRITE_DONE), b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    let ok = b.icmp(IntCond::Eq, st, dn);
    b.cond_br(ok, (l_on, &[]), (l_bad, &[]));
    b.switch_to(l_bad);
    let (neg, uns) = (b.icmp(IntCond::Slt, st, env.zero), b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
    let err = b.select(neg, st, uns);
    stop_at(b, x, row, err);
    b.switch_to(l_on);
    let c = rerun.rerun(b, env, row, site, node, &mut |b, env, r| {
        let v = cells(b, env, r);
        store(b, env, &v);
        let st = host_op(b, env, site, op);
        // the status goes back to the op's arms; the join below is never taken from here (the rerun's own join)
        let (t, l_never) = (b.bconst(true), b.create_block(&[]));
        b.cond_br(t, (l_st, &[st]), (l_never, &[]));
        b.switch_to(l_never);
        row.carry.copy_in(env.mcx)
    });
    b.br(l_join, &c.params());
}
