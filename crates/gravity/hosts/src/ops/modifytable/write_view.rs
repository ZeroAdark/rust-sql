//! Rung 10, member `TargetView`: an INSERT into a view — its INSTEAD OF INSERT row trigger takes each formed row
//! (`ExecIRInsertTriggers` over a virtual slot of the view's row); a NULL return skips the row. No heap, index or constraint.

use std::rc::Rc;

use datum::Datum;
use mcx::Mcx;
use trigger::{TriggerFmgrCache, TriggerWhenCache, TriggerWhenEval};
use types_error::{PgError, PgResult, ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE};
use types_rel::Relation;
use types_slot::SlotData;
use types_trigger::TriggerDesc;

use crate::scratch::NodeScratch;

pub struct ViewInsert {
    desc: Rc<TriggerDesc<'static>>,
    fmgr: TriggerFmgrCache,
    when: TriggerWhenCache<'static>,
    new: SlotData<'static>,
    /// D667: the last done row as RETURNING reads it (the row the INSTEAD OF trigger returned); by-reference cells point
    /// into the slot's retained buffer, kept until the next row's `insert` clears it.
    words: Vec<u64>,
    wnulls: Vec<u8>,
    done: bool,
    scratch: NodeScratch,
}

impl ViewInsert {
    /// `CheckValidResultRel` for a view target: without the INSTEAD OF INSERT row trigger the statement is the executor's 55000
    /// (`merge`: MERGE's hint, rewriteHandler.c `error_view_not_updatable`).
    pub fn bind(view: &Relation<'static>, merge: bool) -> PgResult<ViewInsert> {
        let desc = if view.rd_hastriggers { relcache_seams::relation_get_trigger_desc::call(view.rd_id)? } else { None };
        let Some(desc) = desc.filter(|d| d.trig_insert_instead_row) else {
            return Err(Box::new(PgError::error(format!("cannot insert into view \"{}\"", view.name())) // alloc: Cold
                    .with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                    .with_hint(if merge { "To enable inserting into the view using MERGE, provide an INSTEAD OF INSERT trigger." } else { "To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule." }),
            ));
        };
        let scratch = NodeScratch::new("GravityViewInsert");
        let new = tableam::table_slot_create(scratch.mcx(), view)?;
        Ok(ViewInsert { desc, fmgr: TriggerFmgrCache::default(), when: TriggerWhenCache::default(), new, words: Vec::new(), wnulls: Vec::new(), done: false, scratch }) // alloc: Node
    }

    /// The row's cells into the view's slot, the triggers' verdict: false = a NULL return, the row is skipped (not counted).
    pub fn insert(&mut self, mcx: Mcx<'static>, view: &Relation<'static>, values: &[Datum], nulls: &[bool]) -> PgResult<bool> {
        self.done = false;
        exectuples::exec_clear_tuple(&mut self.new, self.scratch.mcx());
        let n = values.len();
        let base = self.new.base_mut();
        base.tts_values[..n].copy_from_slice(values);
        base.tts_isnull[..n].copy_from_slice(nulls);
        exectuples::exec_store_virtual_tuple(&mut self.new);
        // by-ref cells into the slot's retained buffer in the host context — the trigger's own materialize would put it in `mcx` (reset per row)
        exectuples::exec_materialize_slot(&mut self.new, self.scratch.mcx())?;
        self.new.base_mut().tts_tableOid = view.rd_id;
        let desc = Rc::clone(&self.desc);
        let ViewInsert { fmgr, when, new, scratch, .. } = self;
        let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: None };
        let go = trigger::ExecIRInsertTriggers(mcx, view, &desc, fmgr, &mut w, new)?;
        if go {
            // D667: ExecInsert's RETURNING projects the row the trigger returned: copied into the slot's retained buffer
            // (the trigger's tuple is the per-row context's), its cells kept for `written`
            exectuples::exec_materialize_slot(&mut self.new, self.scratch.mcx())?;
            exectuples::slot_getallattrs(&mut self.new);
            let base = self.new.base();
            let natts = view.rd_att.natts as usize;
            self.words.clear();
            self.wnulls.clear();
            // bounded("key-columns: one word per target column, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..natts.min(base.tts_nvalid as usize) {
                self.words.push(base.tts_values[k].as_u64()); // alloc: Node
                self.wnulls.push(base.tts_isnull[k] as u8); // alloc: Node
            }
            self.done = true;
        }
        Ok(go)
    }

    /// D667: the last done row's cells for RETURNING (the INSTEAD OF trigger's row).
    pub fn written(&self) -> Option<(&[u64], &[u8])> {
        self.done.then(|| (&self.words[..], &self.wnulls[..]))
    }

    /// A MERGE's other action wrote the statement's latest row: this host's last row is no longer RETURNING's.
    pub fn forget(&mut self) {
        self.done = false;
    }

    /// D636: the row the INSTEAD OF INSERT trigger returned (ExecIRInsertTriggers leaves it in `new`) into the caller's
    /// cells, for ExecInsert's view check over it; by-reference cells point into the slot's image, alive until `row_done`.
    pub fn take_row(&mut self, values: &mut [Datum], nulls: &mut [bool]) {
        exectuples::slot_getallattrs(&mut self.new);
        let base = self.new.base();
        let n = values.len().min(base.tts_nvalid as usize);
        values[..n].copy_from_slice(&base.tts_values[..n]);
        nulls[..n].copy_from_slice(&base.tts_isnull[..n]);
    }

    /// The row's per-call memory goes; the slot keeps the done row for RETURNING (`written`) until the next `insert`.
    pub fn row_done(&mut self) {
        self.scratch.reset_exec();
    }
}
