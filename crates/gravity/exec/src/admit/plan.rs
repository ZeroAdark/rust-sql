//! The planning context of one admission: a `GravityPlan` aset context under the admissions' root, owned by a plan child
//! of the running STATEMENT (`op_begin(statement_owner(), LABEL_PLAN, 0)`, ended after the context drops). Made at the
//! first use, so an admission that plans nothing (a hit) makes none. Its scratch is on the statement's plan row by
//! construction and leaves with the admission.
use std::cell::OnceCell;

use gravity_driver::mem::root::Root;
use mcx::{Mcx, MemoryContext};
use memledger::charge::{op_begin, op_end, owner_ctx, statement_owner, LABEL_PLAN};
use memledger::Owner;

/// The context drops before the owner's end (field order), so the owner ends drained.
struct PlanCtx {
    ctx: MemoryContext,
    owner: OwnerEnd,
}

struct OwnerEnd(Owner);

impl Drop for OwnerEnd {
    fn drop(&mut self) {
        op_end(self.0);
    }
}

pub struct PlanScope<'r> {
    root: &'r Root,
    cx: OnceCell<PlanCtx>,
}

impl<'r> PlanScope<'r> {
    pub fn new(root: &'r Root) -> PlanScope<'r> {
        PlanScope { root, cx: OnceCell::new() }
    }

    pub fn mcx(&self) -> Mcx<'_> {
        self.cx
            .get_or_init(|| {
                let owner = op_begin(statement_owner(), LABEL_PLAN, 0);
                let ctx = self.root.new_child("GravityPlan");
                ctx.set_owner(owner);
                PlanCtx { ctx, owner: OwnerEnd(owner) }
            })
            .ctx
            .mcx()
    }

    /// The report line's `plan_peak=` / `plan_held=`: the context's peak chunk bytes and the bytes on the plan owner's row
    /// (`owner_ctx` reads one cell; `usage` walks every operator cell); zeros when nothing planned.
    pub fn stats(&self) -> (usize, i64) {
        self.cx.get().map_or((0, 0), |c| {
            let (named, inferred) = owner_ctx(c.owner.0);
            (c.ctx.subtree_peak(), named + inferred)
        })
    }
}
