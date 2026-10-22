//! The written row against its relation's partition constraint: `ExecPartitionCheck` on a partition written
//! directly (no routing: a row outside the bound is the error, never a move); the qual's state per bind.

use datum::Datum;
use mcx::{Mcx, PgBox};
use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED};
use types_rel::Relation;
use types_slot::SlotData;

use crate::ops::modifytable::write_constraints::ModifiedCols;

pub struct PartitionCheck {
    state: Option<PgBox<'static, execexpr::ExprState<'static>>>,
    slot: Option<SlotData<'static>>,
}

impl PartitionCheck {
    pub fn new() -> PartitionCheck {
        PartitionCheck { state: None, slot: None }
    }

    pub fn reset(&mut self) {
        self.state = None;
    }

    /// `on_conflict_update`: ExecCrossPartitionUpdate's first error, the DO UPDATE row leaving its partition; `root`: a routed leaf's root (the DETAIL row in its layout).
    /// The qual's state is built in `bind_mcx` (reset only at unbind, after `reset`), never in `row_mcx`: that one is reset
    /// after every row, so a state built there dangles from the second row on (D778).
    pub fn check(&mut self, mcx: Mcx<'static>, bind_mcx: Mcx<'static>, row_mcx: Mcx<'static>, rel: &Relation<'static>, values: &[Datum], nulls: &[bool], modified: Option<ModifiedCols>, on_conflict_update: bool, root: Option<&Relation<'static>>) -> PgResult<()> {
        if !rel.rd_rel.relispartition {
            return Ok(());
        }
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, rel)?); // alloc: Node
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
        let eval_mcx = if self.state.is_none() { bind_mcx } else { row_mcx };
        let r = match execpartition::exec_partition_check(eval_mcx, &mut self.state, rel, slot) {
            Err(e) => Err(e),
            Ok(true) => Ok(()),
            Ok(false) if on_conflict_update => Err(Box::new(PgError::error("invalid ON UPDATE specification").with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED).with_detail("The result tuple would appear in a different partition than the original tuple."))), // alloc: Cold
            Ok(false) => Self::violation(row_mcx, rel, slot, modified, root),
        };
        exectuples::exec_clear_tuple(slot, mcx);
        r
    }

    /// H-e (c): ExecUpdateAct's `partition_constraint_failed` test alone (no error): false = the row leaves its partition
    /// and the move host takes it (`write_move.rs`); a relation that is not a partition passes.
    pub fn passes(&mut self, mcx: Mcx<'static>, bind_mcx: Mcx<'static>, row_mcx: Mcx<'static>, rel: &Relation<'static>, values: &[Datum], nulls: &[bool]) -> PgResult<bool> {
        if !rel.rd_rel.relispartition {
            return Ok(true);
        }
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, rel)?); // alloc: Node
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
        let eval_mcx = if self.state.is_none() { bind_mcx } else { row_mcx };
        let r = execpartition::exec_partition_check(eval_mcx, &mut self.state, rel, slot);
        exectuples::exec_clear_tuple(slot, mcx);
        r
    }

    /// ExecPartitionCheckEmitError: the DETAIL over insertedCols ∪ updatedCols.
    #[cold]
    fn violation(row_mcx: Mcx<'static>, rel: &Relation<'static>, slot: &mut SlotData<'static>, modified: Option<ModifiedCols>, root: Option<&Relation<'static>>) -> PgResult<()> {
        let union = match modified {
            Some((ins, upd)) => Some(ins.union(upd, row_mcx)?), // alloc: Cold
            None => None,
        };
        Err(execpartition::partition_constraint_violation(row_mcx, rel, slot, union.as_ref(), root))
    }
}

impl Default for PartitionCheck {
    fn default() -> PartitionCheck {
        PartitionCheck::new()
    }
}
