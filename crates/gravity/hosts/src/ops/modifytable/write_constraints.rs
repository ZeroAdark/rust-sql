//! The written row's CHECK and NOT NULL constraints through the incumbent, over a virtual slot of
//! the datums the tuple is formed from (docs/gravity/lower.md §14.12, hosts.md).

use datum::Datum;
use mcx::Mcx;
use nodemodifytable::{CheckExpr, VirtualNnExpr};
use types_error::PgResult;
use types_rel::Relation;
use types_slot::SlotData;

pub struct WriteConstraints {
    armed: Option<bool>,
    checks: Option<mcx::PgVec<'static, CheckExpr<'static>>>,
    virtual_nn: Option<mcx::PgVec<'static, VirtualNnExpr<'static>>>,
    slot: Option<SlotData<'static>>,
    /// insertedCols ∪ updatedCols when both are set (ON CONFLICT DO UPDATE), built once per bind.
    union: Option<types_nodes::Bitmapset<'static>>,
}

/// The statement's `insertedCols` / `updatedCols` (the result relation's RTEPermissionInfo, in the planned statement):
/// `ExecConstraints`' `modifiedCols`, what a failing row's DETAIL may show a user without table SELECT.
pub type ModifiedCols = (&'static types_nodes::Bitmapset<'static>, &'static types_nodes::Bitmapset<'static>);

impl WriteConstraints {
    pub fn new() -> WriteConstraints {
        WriteConstraints { armed: None, checks: None, virtual_nn: None, slot: None, union: None }
    }

    /// Neither constraint kind = nothing to read per row (a domain's are the input function's).
    fn arm(rel: &Relation<'_>) -> bool {
        rel.rd_att.constr.as_deref().is_some_and(|c| c.has_not_null || c.num_check > 0 || c.relchecks > 0)
    }

    pub fn reset(&mut self) {
        self.armed = None;
        self.checks = None;
        self.virtual_nn = None;
        self.slot = None;
        self.union = None;
    }

    /// `mcx` holds the states and slot for the bind (the host's per-bind context, reset at unbind: H-i), `row_mcx` the evaluation's garbage; `root` = a routed row's root (the DETAIL's relation).
    pub fn check(&mut self, mcx: Mcx<'static>, row_mcx: Mcx<'_>, rel: &Relation<'static>, values: &[Datum], nulls: &[bool], modified: Option<ModifiedCols>, root: Option<&Relation<'static>>) -> PgResult<()> {
        if !*self.armed.get_or_insert_with(|| Self::arm(rel)) {
            return Ok(());
        }
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, rel)?); // alloc: Exec
        }
        let slot = self.slot.as_mut().expect("just created");
        exectuples::exec_clear_tuple(slot, mcx);
        {
            let base = slot.base_mut();
            let n = values.len().min(base.tts_values.len());
            base.tts_values[..n].copy_from_slice(&values[..n]);
            base.tts_isnull[..n].copy_from_slice(&nulls[..n]);
        }
        exectuples::exec_store_virtual_tuple(slot);
        slot.base_mut().tts_tableOid = rel.rd_id;
        let cols = match modified {
            Some((ins, upd)) if upd.is_empty() => Some(ins),
            Some((ins, upd)) if ins.is_empty() => Some(upd),
            Some((ins, upd)) => {
                if self.union.is_none() {
                    self.union = Some(ins.union(upd, mcx)?); // alloc: Exec
                }
                self.union.as_ref()
            }
            None => None,
        };
        let r = nodemodifytable::exec_constraints_in_row(mcx, row_mcx, &mut self.checks, &mut self.virtual_nn, rel, slot, root, cols);
        exectuples::exec_clear_tuple(slot, mcx);
        r
    }
}

impl Default for WriteConstraints {
    fn default() -> WriteConstraints {
        WriteConstraints::new()
    }
}
