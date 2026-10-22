//! modifytable's premises (v5 landing 4, moved from props/write.rs): the row op's trigger need (M4-L0 L0-j): which
//! commands' triggers a row op runs, and `write.transition` when one of them fires on a relation the row op writes;
//! `write.lock` for a write op's own row marks (V4R-227). The props walk calls them per pipeline.
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::{Op, PipelineNode};
use gravity_program::Cmd;

use crate::applies::Prop;
use crate::face::kinds::KindId;

use crate::props::Walk;

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::Write: the write roads (route, several targets, row triggers) and its RETURNING / ON CONFLICT kinds
    /// (moved from props/ops.rs's write arm).
    pub(crate) fn write(&mut self, w: &WriteDesc<'p>) {
        // V4R-5: several result relations, or a cross-partition UPDATE (a DELETE + routed INSERT even with one
        // target), are write.route's
        if w.targets.len() > 1 || w.members.has(Member::Targets) || w.members.has(Member::CrossPartition) {
            self.p.add(Prop::MultiTarget);
        }
        // V4R-204 (M4 Decision 2): an INSERT / MERGE routed through a partitioned parent is write.route's; a BEFORE /
        // INSTEAD OF ROW INSERT trigger on the target (or unknown facts) is a per-row turn
        if w.members.has(Member::Route) && matches!(w.cmd, Cmd::Insert | Cmd::Merge) {
            self.p.add(Prop::Route);
        }
        let t = self.gx.facts.trig_ext(w.plan.target_rel);
        if !t.known || (t.before_row | t.instead_row) & TRIG_INSERT != 0 {
            self.p.add(Prop::RowTrigger);
        }
        if let Some(r) = w.returning {
            self.add(KindId::WriteReturning);
            self.list(r);
        }
        if let Some(c) = &w.on_conflict {
            self.add(KindId::WriteOnconflict);
            self.list(c.set);
            // DO UPDATE's WHERE (onConflictWhere): its expressions' kinds too (a SubPlan there is nest.subplan)
            if let Some(f) = c.filter {
                self.expr(f);
            }
        }
    }

    /// A row op whose written relation (or its partition root) has a trigger on a command it runs, BEFORE / AFTER / INSTEAD
    /// OF ROW, statement or transition capture (disabled ones and RI's action triggers included), or unknown trigger facts,
    /// needs write.transition (V4-PLAN decision 1). A cross-partition UPDATE also writes the leaves under its
    /// root: each is checked, and leaves the facts source cannot list count as firing. Rules never move a statement.
    pub(crate) fn triggers(&mut self, p: &PipelineNode<'p>, cmd: u8) {
        let Some(w) = p.ops.iter().find_map(|op| if let Op::Write(w) = op { Some(w) } else { None }) else { return };
        let bits = runs(w, cmd);
        let fires = |t: TrigExt| !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & bits != 0;
        let facts = self.gx.facts;
        let mut hit = fires(facts.trig_ext(w.plan.target_rel)) || w.route.is_some_and(|r| fires(facts.trig_ext(r.root_relid)));
        if !hit && cross(w) {
            // the moved row's leaves: any leaf under the root may take it; leaves the source cannot list count as firing
            let known = w.route.is_some_and(|r| facts.partition_leaves(r.root_relid, &mut |l| hit |= fires(l.trig)));
            hit |= !known;
        }
        if hit {
            self.add(KindId::WriteTransition);
        }
    }

    /// V4R-227: a write op carrying its own row marks (a join's or a rule's REFERENCE / COPY marks for EvalPlanQual over the
    /// other relations, or a locking mark) needs write.lock's join form: the row ops refuse it by that name (write/delete.rs,
    /// write/insert.rs), so the census places the statement there, not where its sources are.
    pub(crate) fn marks(&mut self, p: &PipelineNode<'p>) {
        if p.ops.iter().any(|op| matches!(op, Op::Write(w) if !w.marks.is_nil() || !w.mark_descs.is_empty())) {
            self.add(KindId::WriteLock);
        }
    }
}

/// L0-j: the trigger commands a row op of command bits `cmd` runs (a missed trigger is a WRONG, not a refusal): INSERT ...
/// ON CONFLICT DO UPDATE updates the conflicting row (ExecOnConflictUpdate: BEFORE / AFTER UPDATE ROW, and the UPDATE
/// statement triggers); a cross-partition UPDATE deletes the row from its leaf and inserts it through the root
/// (ExecCrossPartitionUpdate: the DELETE and INSERT row triggers of the leaves).
fn runs(w: &WriteDesc<'_>, cmd: u8) -> u8 {
    let mut bits = cmd;
    if w.members.has(Member::ConflictUpdate) {
        bits |= TRIG_UPDATE;
    }
    if cross(w) {
        bits |= TRIG_DELETE | TRIG_INSERT;
    }
    bits
}

/// A cross-partition UPDATE (member CrossPartition, the route's `cross-partition`): a DELETE plus a routed INSERT.
fn cross(w: &WriteDesc<'_>) -> bool {
    w.members.has(Member::CrossPartition) || w.route.is_some_and(|r| r.cross_partition)
}
