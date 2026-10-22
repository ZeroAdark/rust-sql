//! LockRows: the statement's locking marks on one serial op over the child.
use gravity_program::{Reason, WritePlan};
use types_nodes::plannodes::LockRows;

use crate::admit::Refusal;
use crate::graph::Op;
use crate::ledger::Kind;
use crate::ops;
use crate::walk::{Builder, Open};

use super::{marks_of, serial_after, Carried, LockDesc, MemberSet};

pub(crate) fn map_lock_rows<'p>(b: &mut Builder<'p, '_>, lr: &'p LockRows<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&lr.plan, Kind::LockRows);
    let id = plan.plan_node_id;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on LockRows"));
    }
    let mut c = Carried { set: MemberSet::EMPTY };
    let mut plan_lock: Option<WritePlan> = None;
    let marks = marks_of(b, &lr.plan, &lr.rowMarks, k, id, &mut c, Some(&mut plan_lock))?;
    let mut open = b.child(plan, k)?;
    // a LockRows whose every mark is REFERENCE or COPY (FOR UPDATE of a subquery PG plans apart, its own LockRows taking the
    // locks): ExecLockRows locks nothing and never starts its EvalPlanQual (lr_arowMarks is empty), so the child's rows pass;
    // so does one over a childless partitioned table's dummy rel (`marks_of`), whose child returns no row
    let Some(plan_lock) = plan_lock else {
        serial_after(&mut open, Reason::Lock);
        ops::project(&mut open, &plan.targetlist);
        b.graph.note(Kind::PlanRowMark);
        b.graph.note(k);
        return Ok(open);
    };
    serial_after(&mut open, Reason::Lock);
    open.push(Op::Lock(LockDesc { marks, plan: plan_lock, epq: lr.epqParam, tlist: &plan.targetlist, members: c.set }));
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(Kind::PlanRowMark);
    b.graph.note(k);
    Ok(open)
}
