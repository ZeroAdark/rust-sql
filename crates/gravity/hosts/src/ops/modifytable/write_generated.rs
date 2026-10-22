//! Rung 10, member `Generated`: the STORED generated columns computed by the incumbent's `ExecComputeStoredGenerated`
//! over a virtual slot of the row's cells, after the BEFORE ROW prologue and before the constraints and the index entries.

use datum::Datum;
use mcx::Mcx;
use nodemodifytable::GeneratedExpr;
use types_error::PgResult;
use types_rel::Relation;
use types_slot::SlotData;

pub struct StoredGenerated {
    armed: Option<bool>,
    exprs: Option<mcx::PgVec<'static, GeneratedExpr<'static>>>,
    slot: Option<SlotData<'static>>,
    words: Vec<u64>,
    wnulls: Vec<u8>,
    replaced: bool,
}

const STORED: i8 = types_core::catalog::ATTRIBUTE_GENERATED_STORED as i8;

impl StoredGenerated {
    pub fn new() -> StoredGenerated {
        StoredGenerated { armed: None, exprs: None, slot: None, words: Vec::new(), wnulls: Vec::new(), replaced: false } // alloc: Node
    }

    fn arm(rel: &Relation<'_>) -> bool {
        rel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) || (0..rel.rd_att.natts as usize).any(|i| rel.rd_att.attr(i).attgenerated == STORED)
    }

    pub fn reset(&mut self, mcx: Mcx<'static>) {
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, mcx);
        }
        self.armed = None;
        self.exprs = None;
        self.slot = None;
        self.replaced = false;
    }

    /// H-c (D236): the last computed row's cells, stored generated columns included (the written row: they are computed
    /// after the BEFORE ROW triggers, over the trigger's row); `None` when this host computed none for it.
    pub fn written(&self) -> Option<(&[u64], &[u8])> {
        self.replaced.then(|| (&self.words[..], &self.wnulls[..]))
    }

    /// A row op that computes nothing (a DELETE, MERGE's included): no computed row is this row's.
    pub fn forget(&mut self) {
        self.replaced = false;
    }

    pub fn take_words(&mut self) -> Option<(Vec<u64>, Vec<u8>)> {
        self.replaced.then(|| (core::mem::take(&mut self.words), core::mem::take(&mut self.wnulls)))
    }

    pub fn keep(&mut self, w: (Vec<u64>, Vec<u8>)) {
        self.words = w.0;
        self.wnulls = w.1;
    }

    pub fn compute(&mut self, mcx: Mcx<'static>, row_mcx: Mcx<'_>, rel: &Relation<'static>, values: &mut [Datum], nulls: &mut [bool]) -> PgResult<()> {
        self.replaced = false;
        if !*self.armed.get_or_insert_with(|| Self::arm(rel)) {
            return Ok(());
        }
        if self.slot.is_none() {
            // a virtual slot: its materialized row goes into the slot's retained buffer, reused row after row. The
            // relation's own slot kind formed a new heap image per row, which the per-bind context (a bump context: a
            // free is a no-op) kept until unbind, where PG pfrees it at the next clear.
            self.slot = Some(exectuples::make_tuple_table_slot(mcx, types_slot::TupleSlotKind::Virtual, Some(rel.rd_att.clone()))); // alloc: Exec
        }
        let slot = self.slot.as_mut().expect("just created");
        exectuples::exec_clear_tuple(slot, mcx);
        let n = values.len().min(slot.base().tts_values.len());
        {
            let base = slot.base_mut();
            base.tts_values[..n].copy_from_slice(&values[..n]);
            base.tts_isnull[..n].copy_from_slice(&nulls[..n]);
        }
        exectuples::exec_store_virtual_tuple(slot);
        slot.base_mut().tts_tableOid = rel.rd_id;
        nodemodifytable::exec_compute_stored_generated_in_row(mcx, row_mcx, &mut self.exprs, rel, slot)?;
        exectuples::slot_getallattrs(slot);
        let base = slot.base();
        self.words.clear();
        self.wnulls.clear();
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k];
            nulls[k] = base.tts_isnull[k];
            self.words.push(values[k].as_u64()); // alloc: Node
            self.wnulls.push(nulls[k] as u8); // alloc: Node
        }
        self.replaced = true;
        Ok(())
    }
}

impl Default for StoredGenerated {
    fn default() -> StoredGenerated {
        StoredGenerated::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H-c (D236): the computed row is the written row until a row op computes nothing (a DELETE, MERGE's included).
    #[test]
    fn the_computed_row_is_handed_back_until_a_row_op_computes_none() {
        let mut g = StoredGenerated::new();
        assert_eq!(g.written(), None, "nothing computed yet");
        g.words = vec![7, 14]; // alloc: Cold
        g.wnulls = vec![0, 0]; // alloc: Cold
        g.replaced = true;
        let taken = g.take_words().expect("computed");
        g.keep(taken);
        assert_eq!(g.written(), Some((&[7u64, 14][..], &[0u8, 0][..])), "the row as written, after the op's take and keep");
        g.forget();
        assert_eq!(g.written(), None, "a DELETE after an INSERT in one host (MERGE) hands back no computed row");
    }
}
