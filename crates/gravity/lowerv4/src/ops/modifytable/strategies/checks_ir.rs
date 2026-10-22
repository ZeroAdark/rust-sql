//! Road `write.checks` (H27; R12-L13; cards/M4.md L3 `write/checks_ir.rs`): the written row's checks, in PostgreSQL's
//! order (ExecConstraints' NOT NULL, then CHECK, then the partition constraint, then ExecWithCheckOptions' view / RLS
//! checks) with PostgreSQL's errors: SQLSTATE, message and the failing row's detail. Owner M4-INSERT.
//!
//! Where they run (`ChecksAt`): at M4's first landing every check runs in the host op, inside the served statement's
//! `write_row` call (a served mechanism, not a fallback): hosts/src/write_constraints.rs (NOT NULL, CHECK,
//! through the incumbent ExecConstraints), write_generated.rs (STORED generated columns computed before the checks, as
//! ExecComputeStoredGenerated), write_partition.rs (a directly written partition's constraint) and write_check.rs
//! (WITH CHECK OPTION / RLS). A NOT NULL / CHECK form compiled over the lanes (`ChecksAt::Ir`) comes only if the thin
//! cells show the host call costs, forecast first.
//!
//! What the host cannot check refuses here by name, the ladder's `unservable_check` / `unmaintainable` (lower/src/write.rs
//! at ladder-final): a check option whose qual holds a Param other than an InitPlan's, a check option of a routed target,
//! and an index the host's maintenance refuses (an access method other than btree / hash / gin / gist / spgist / brin, a unique non-btree index without
//! exclusion operators), and any index with no frozen IndexPlan (D624) but one a concurrent DDL left not valid or not ready. The
//! host's IndexMaint maintains only the plan's indexes.
//! M8-WRITE W2: an InitPlan's parameter in a check option's qual is the host's, lazily at the qual's read, as
//! PostgreSQL's ExecEvalParamExec runs the InitPlan (hosts write_check.rs `InitParams`: the Init cell's run, M8-SUB's
//! nest.initplan): an InitPlan the qual never reads (an OR's other arm passed) never runs and never raises.
//! W6-D1112 (V4R-528): a SubPlan in an INSERT's or UPDATE's check option is the host's too, read at the qual's read and
//! evaluated by the row op step (write/rowop_sub.rs, which refuses the forms it does not serve by name); a write with a
//! SubPlan in ON CONFLICT or a MERGE action keeps D1112 (`check_only`); one in RETURNING beside the checks' is served as
//! each is alone (write.returning `returning_and_checks`, rxnested).
//! decompose's `IndexDesc::plain()` is false for a partial, expression, INCLUDE or deferrable index, which therefore gets
//! no IndexPlan. A btree one of these is left unbound by the decomposer (its own member; Decision 18, after H-a). But
//! `index_member` classifies a non-btree or exclusion index by its access method or EXCLUDE first, so a partial gist
//! index or an EXCLUDE ... WHERE reaches here bound, and is refused here by name. A deferrable EXCLUDE is plain / general
//! (V4R-297: the host probes it CEOUC_NOWAIT and queues the recheck).
use gravity_decompose::ops::shared::modifytable::{CheckOpt, WriteDesc};
use gravity_decompose::IndexAm;
use gravity_program::Cmd;
use types_nodes::parsenodes::WCOKind;
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeTag};

use crate::face::end::ChecksAt;
use crate::face::kinds::KindId;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

crate::strategies! {
    /// Checks / generated / partial / expression keys in IR.
    WRITE_CHECKS = 118, "write.checks", InsertUpdate;
}

pub(crate) const ROWS: Table = Table { strategies: &[(WRITE_CHECKS, &Checks)], ..crate::ops::NONE };

pub(crate) struct Checks;

impl Checks {
    /// Where write `w`'s checks run, or a refusal by name at `node`.
    pub(crate) fn plan(&self, gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>) -> ChecksAt {
        if let Some(why) = w.checks.iter().find_map(|c| unservable(gx, w, c)) {
            // a SubPlan in the qual: ExecInitQual under the ModifyTable, the host has no SubPlanState (nest.subplan, M8)
            if why == SUBPLAN {
                shape(node, "a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)");
            }
            shape(node, why);
        }
        for t in w.targets.iter() {
            // D624: an index a concurrent CREATE / DROP / REINDEX CONCURRENTLY left not valid or not ready has no frozen
            // IndexPlan; IndexMaint's list check sends the write to the executor's own path (D765's generic), which
            // maintains a ready one and skips a not-ready one as ExecInsertIndexTuples does
            let pending = |i: &gravity_decompose::ops::shared::modifytable::IndexDesc| {
                let e = gx.facts.index_ext(i.indexid);
                e.known && !(e.valid && e.ready) && !t.plan.indexes().iter().any(|p| p.index_oid == i.indexid)
            };
            for i in t.indexes.iter().filter(|i| !pending(i)) {
                if let Some(why) = unmaintainable(i) {
                    shape(node, why);
                }
                // D624: no IndexPlan, so IndexMaint would leave it without the row's entry (or skip its exclusion check)
                if !i.plain() && !i.general() {
                    shape(node, "a partial, expression, INCLUDE or deferrable index of the written relation: no frozen IndexPlan (index_maint, D624)");
                }
            }
            if t.plan.indexes().len() != t.indexes.iter().filter(|i| !pending(i)).count() {
                shape(node, "an index of the written relation outside the frozen plan (index_maint)");
            }
        }
        ChecksAt::Host
    }
}

const SUBPLAN: &str = "a check option qual with a subplan";

/// W6-D1112: whether write `w`'s SubPlans are all its check options' (an INSERT's or UPDATE's, none in RETURNING, ON
/// CONFLICT or a MERGE action): the host reads those (write/rowop_sub.rs); any other keeps D1112. D1112 (rxnested): an
/// INSERT's ON CONFLICT DO NOTHING too (no list of its own: the host's conflict path parks at the RLS insert check and the
/// view check as the plain INSERT's does, hosts write.rs `insert_on_conflict`); DO UPDATE keeps D1112.
pub(crate) fn check_only(w: &WriteDesc<'_>) -> bool {
    let ret = |r: Option<&types_nodes::NodeList<'_>>| r.is_some_and(|r| r.iter().any(gravity_decompose::expr::has_subplan));
    let conflict = w.on_conflict.as_ref().is_none_or(|oc| w.cmd == Cmd::Insert && !oc.update);
    matches!(w.cmd, Cmd::Insert | Cmd::Update) && conflict && w.merge.is_none() && !ret(w.returning) && !w.targets.iter().any(|t| ret(t.returning))
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// `hosts::index_maint::unmaintainable`'s conditions off the decomposer's classification (validity: `pending`, D624).
fn unmaintainable(i: &gravity_decompose::ops::shared::modifytable::IndexDesc) -> Option<&'static str> {
    if !i.insertable {
        Some("an index whose access method is not btree / hash / gin / gist / spgist / brin / bloom (index_maint)")
    } else if !matches!(i.am, IndexAm::Btree) && i.unique && !i.exclusion {
        Some("a unique constraint on a non-btree index of the written relation without exclusion operators (index_maint)")
    } else {
        None
    }
}

/// A check option the host does not serve: members CheckRls / CheckView are served for an RLS INSERT / UPDATE or view qual
/// of the one target, free of SubPlans and Params; under MERGE also the target row's MERGE UPDATE / DELETE USING checks
/// (H-f: the host's merge_check before the action); under ON CONFLICT DO UPDATE the existing row's USING checks too
/// (WCO_RLS_CONFLICT_CHECK, D661-c: the arm calls the host's check after its WHERE, onconflict.rs). D636: over a view target the host checks
/// the row the INSTEAD OF trigger returned (hosts write.rs, ExecInsert / ExecUpdate's view check after
/// ExecIRInsertTriggers / ExecIRUpdateTriggers).
fn unservable(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, c: &CheckOpt<'_>) -> Option<&'static str> {
    let (mut param, mut subplan) = (false, false);
    // W2: an InitPlan's setParam is the host's lazy read (hosts write_check.rs); any other Param refuses by name
    let init = |n: Node<'_>| n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && crate::rerun::params::producer(gx, p.paramid) == Some(KindId::NestInitplan));
    let mut clause = |q: Node<'_>| {
        subplan |= gravity_decompose::expr::has_subplan(q);
        // W6-D1112: a SubPlan's own parameters (an ANY / ALL test's) are write/rowop_sub.rs's
        gravity_decompose::expr::walk(q, &mut |n| {
            param |= n.node_tag() == NodeTag::T_Param && !init(n);
            !param && n.node_tag() != NodeTag::T_SubPlan
        });
    };
    match c.qual.as_list() {
        Some(l) => l.iter().for_each(&mut clause),
        None => clause(c.qual),
    }
    let conflict_update = w.on_conflict.as_ref().is_some_and(|oc| oc.update);
    // an UPDATE of several result relations: each target's list is its own, in its layout (the multi-target host hands
    // each target's host its list, hosts write_route.rs `MultiTargetHost`). G2(d): so does a MERGE of several whose
    // actions update or delete and never insert (the same host over its MergeOps, W6-MERGE-ROUTE-MATCHED); an
    // INSERT-only MERGE through the root checks each routed row by the first result relation's list mapped onto its leaf
    // (hosts RouteHost `checks_of`, merge/nm.rs `route_plan`; ExecInitPartitionInfo), the other lists being the same quals
    let merge = w.merge.as_ref().is_some_and(|m| !m.actions.iter().any(|a| a.cmd == Some(Cmd::Insert)) || crate::ops::modifytable::merge::nm::insert_only(m));
    let several = w.targets.len() > 1 && (w.cmd == Cmd::Update && w.merge.is_none() || w.cmd == Cmd::Merge && merge);
    if c.target != 0 && !several {
        Some("a check option of a routed target")
    } else if !matches!(c.kind, WCOKind::WCO_VIEW_CHECK | WCOKind::WCO_RLS_INSERT_CHECK | WCOKind::WCO_RLS_UPDATE_CHECK)
        && !(conflict_update && matches!(c.kind, WCOKind::WCO_RLS_CONFLICT_CHECK))
        // H-f (V4R-239): ExecMergeMatched's USING checks of the target's current version, in the host's merge_check
        && !(w.merge.is_some() && matches!(c.kind, WCOKind::WCO_RLS_MERGE_UPDATE_CHECK | WCOKind::WCO_RLS_MERGE_DELETE_CHECK))
    {
        Some("a check option kind of ON CONFLICT / MERGE")
    } else if subplan && !check_only(w) && !crate::ops::modifytable::returning::returning_and_checks(w) {
        Some(SUBPLAN)
    } else if param {
        Some("a check option qual over a parameter")
    } else {
        None
    }
}
