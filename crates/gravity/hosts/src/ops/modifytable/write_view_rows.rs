//! H-d (M4-plan §6, Decision 8: no change to pgrust main): UPDATE / DELETE of a view,
//! served by the view's INSTEAD OF row triggers in Gravity host code, in `write_view.rs`'s `ViewInsert` pattern.
//! - OLD is the view's `wholerow` junk: the write site's last cell, a composite Datum. As in nodeModifyTable.c
//!   :4480-4491, its `t_self` is invalid and its `t_tableOid` is InvalidOid ("historically, view triggers see
//!   invalid t_tableOid"). A NULL wholerow is PG's "wholerow is NULL" (MERGE's NOT MATCHED arm never calls here).
//! - NEW (UPDATE) is OLD's columns with the SET columns (the statement's updatedCols) replaced by the site's cells:
//!   ExecGetUpdateNewTuple over the old slot. Each trigger's returned row becomes the next trigger's NEW.
//! - A NULL return skips the row: later triggers do not fire, and the row is neither counted nor returned. UPDATE
//!   RETURNING reads the final NEW, DELETE RETURNING reads OLD (`written`).
//! - Statement triggers are the write host's (`trigger_stmt.rs`, with the view as the result relation). Views have
//!   no AFTER ROW triggers, no transition tables, and no WHEN on an INSTEAD OF trigger, so none of those arms exist.
//!
//! Divergence note: this is a second copy of trigger.c `ExecIRUpdateTriggers` (:3220-3285) and
//! `ExecIRDeleteTriggers` (:2854-2898), and of the INSTEAD arm of pgrust's `row_triggers_common` (nodemodifytable
//! `lib.rs:6018`). A change to either, in core or in PG, is mirrored here. The e2e arm (m4_triggers: the triggers /
//! updatable_views / returning INSTEAD OF rows, fallback off, against PG) is the tripwire. As in `ViewInsert`, no
//! `ri_TrigInstrument` is passed: under EXPLAIN ANALYZE, ExecCallTriggerFunc counts each call in the statement's trigger
//! level (trigger crate `instr`, D3381).
//! Created by M4 (owner M4-HOSTS).
use core::ptr::NonNull;
use std::rc::Rc;

use datum::Datum;
use gravity_program::Cmd;
use mcx::Mcx;
use trigger::{ExecCallTriggerFunc, TriggerEnabled, TriggerFmgrCache, TriggerWhenCache, TriggerWhenEval};
use types_error::{PgError, PgResult, ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE};
use types_nodes::Bitmapset;
use types_rel::Relation;
use types_slot::SlotData;
use types_trigger::{TriggerDesc, TRIGGER_EVENT_DELETE, TRIGGER_EVENT_INSTEAD, TRIGGER_EVENT_ROW, TRIGGER_EVENT_UPDATE, TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSTEAD, TRIGGER_TYPE_LEVEL_MASK, TRIGGER_TYPE_ROW, TRIGGER_TYPE_TIMING_MASK, TRIGGER_TYPE_UPDATE};
use types_trigger_call::TriggerData;
use types_tuple::itemptr::ItemPointerSetInvalid;
use types_tuple::{HeapTupleData, ItemPointerData};

use crate::scratch::NodeScratch;

/// `updatedCols`' members are offset by `FirstLowInvalidHeapAttributeNumber` (-7): member `attno + 7`.
const ATTNO_OFFSET: i32 = 7;

pub struct ViewRows {
    desc: Rc<TriggerDesc<'static>>,
    cmd: Cmd,
    fmgr: TriggerFmgrCache,
    when: TriggerWhenCache<'static>,
    old: SlotData<'static>,
    new: SlotData<'static>,
    /// UPDATE's SET columns, 0-based (ExecGetUpdateNewTuple's updateColnos).
    set: Vec<usize>,
    /// The last done row as RETURNING reads it (UPDATE: the final NEW; DELETE: OLD); by-reference cells point into the
    /// row context, reset when the next row starts.
    words: Vec<u64>,
    wnulls: Vec<u8>,
    done: bool,
    /// The row context (`exec`): the fetched and copied tuples, the triggers' per-call memory; the slots' retained
    /// buffers are the node context's, as `ViewInsert`'s.
    scratch: NodeScratch,
}

/// `CheckValidResultRel` for a view target: without the INSTEAD OF row trigger the statement is the executor's 55000
/// (rewriteHandler.c `error_view_not_updatable` with no detail; MERGE's hint names the trigger alone).
fn not_updatable(view: &Relation<'_>, cmd: Cmd, merge: bool) -> Box<PgError> {
    let (msg, hint) = match (cmd, merge) {
        (Cmd::Update, false) => ("cannot update view", "To enable updating the view, provide an INSTEAD OF UPDATE trigger or an unconditional ON UPDATE DO INSTEAD rule."),
        (Cmd::Update, true) => ("cannot update view", "To enable updating the view using MERGE, provide an INSTEAD OF UPDATE trigger."),
        (_, false) => ("cannot delete from view", "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule."),
        (_, true) => ("cannot delete from view", "To enable deleting from the view using MERGE, provide an INSTEAD OF DELETE trigger."),
    };
    Box::new(PgError::error(format!("{msg} \"{}\"", view.name())).with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE).with_hint(hint)) // alloc: Cold
}

/// ExecGetUpdateNewTuple's column list: the `updatedCols` members as 0-based user attributes of `natts`.
pub fn set_columns(updated: &Bitmapset<'_>, natts: usize) -> Vec<usize> {
    (1..=natts as i32).filter(|a| updated.is_member(a + ATTNO_OFFSET)).map(|a| a as usize - 1).collect() // alloc: Node
}

impl ViewRows {
    /// `cmd` is Update or Delete; `updated` the statement's `updatedCols` (UPDATE's SET columns; unused for DELETE and for
    /// MERGE, whose UPDATE cells are the whole new image).
    pub fn bind(view: &Relation<'static>, cmd: Cmd, updated: Option<&Bitmapset<'_>>, merge: bool) -> PgResult<ViewRows> {
        let desc = if view.rd_hastriggers { relcache_seams::relation_get_trigger_desc::call(view.rd_id)? } else { None };
        let fires = |d: &TriggerDesc<'_>| if cmd == Cmd::Update { d.trig_update_instead_row } else { d.trig_delete_instead_row };
        let Some(desc) = desc.filter(|d| fires(d)) else { return Err(not_updatable(view, cmd, merge)) };
        let natts = view.rd_att.natts as usize;
        let set = match (cmd, updated) {
            // H-f: a MERGE UPDATE's cells are the full new image (each action has its own SET list)
            (Cmd::Update, _) if merge => (0..natts).collect(), // alloc: Node
            (Cmd::Update, Some(u)) => set_columns(u, natts),
            (Cmd::Update, None) => return Err(Box::new(PgError::error("gravity: a view UPDATE without its SET columns"))), // alloc: Cold
            _ => Vec::new(), // alloc: Node
        };
        let scratch = NodeScratch::new("GravityViewRows");
        let old = tableam::table_slot_create(scratch.mcx(), view)?;
        let new = tableam::table_slot_create(scratch.mcx(), view)?;
        Ok(ViewRows { desc, cmd, fmgr: TriggerFmgrCache::default(), when: TriggerWhenCache::default(), old, new, set, words: Vec::new(), wnulls: Vec::new(), done: false, scratch }) // alloc: Node
    }

    /// One row: `values` / `nulls` are the view's cells (UPDATE: the SET columns read) and, last, the wholerow Datum.
    /// `Ok(false)` = a trigger returned NULL: the row is skipped (not counted, no RETURNING row).
    pub fn write(&mut self, view: &Relation<'static>, values: &[Datum], nulls: &[bool]) -> PgResult<bool> {
        self.start_row();
        let natts = view.rd_att.natts as usize;
        if values.len() != natts + 1 || nulls.len() != natts + 1 {
            return Err(Box::new(PgError::error("gravity: a view row op without its wholerow cell"))); // alloc: Cold
        }
        if nulls[natts] {
            return Err(Box::new(PgError::error("wholerow is NULL"))); // alloc: Cold
        }
        let rmcx = self.scratch.exec_mcx();
        // a non-null wholerow cell is a live composite image for this call (the site's cell)
        let (img, len) = super::raw::wholerow_image(rmcx, values[natts])?;
        let mut tid = ItemPointerData::default();
        ItemPointerSetInvalid(&mut tid);
        let oldtup = super::raw::tuple_at(img, len, tid, types_core::InvalidOid);
        exectuples::exec_force_store_heap_tuple(oldtup, &mut self.old, rmcx)?;
        exectuples::exec_materialize_slot(&mut self.old, self.scratch.mcx())?;
        let go = if self.cmd == Cmd::Update {
            self.new_from_old(view, &values[..natts], &nulls[..natts])?;
            self.update(view, img, len, tid)?
        } else {
            self.delete(view, img, len, tid)?
        };
        if go {
            let slot = if self.cmd == Cmd::Update { &mut self.new } else { &mut self.old };
            exectuples::slot_getallattrs(slot);
            let base = slot.base();
            self.words.clear();
            self.wnulls.clear();
            // bounded("key-columns: one word per target column, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..natts {
                self.words.push(base.tts_values[k].as_u64()); // alloc: Node
                self.wnulls.push(base.tts_isnull[k] as u8); // alloc: Node
            }
            self.done = true;
        }
        Ok(go)
    }

    /// ExecGetUpdateNewTuple: OLD's columns, the SET columns replaced; materialized in the slot's retained buffer.
    fn new_from_old(&mut self, view: &Relation<'static>, values: &[Datum], nulls: &[bool]) -> PgResult<()> {
        exectuples::slot_getallattrs(&mut self.old);
        exectuples::exec_clear_tuple(&mut self.new, self.scratch.mcx());
        let n = values.len();
        let b = self.old.base();
        let (ov, on) = (&b.tts_values[..n], &b.tts_isnull[..n]);
        let base = self.new.base_mut();
        base.tts_values[..n].copy_from_slice(ov);
        base.tts_isnull[..n].copy_from_slice(on);
        // bounded("key-columns: one cell per SET column of the statement, at most MaxTupleAttributeNumber (1664) columns")
        for &k in self.set.iter().filter(|&&k| k < n) {
            base.tts_values[k] = values[k];
            base.tts_isnull[k] = nulls[k];
        }
        exectuples::exec_store_virtual_tuple(&mut self.new);
        exectuples::exec_materialize_slot(&mut self.new, self.scratch.mcx())?;
        self.new.base_mut().tts_tableOid = view.rd_id;
        Ok(())
    }

    /// trigger.c ExecIRUpdateTriggers: each matching trigger in order, NEW handed from one to the next; a NULL ends it.
    fn update(&mut self, view: &Relation<'static>, img: *const u8, len: u32, tid: ItemPointerData) -> PgResult<bool> {
        let rmcx = self.scratch.exec_mcx();
        let event = TRIGGER_EVENT_UPDATE | TRIGGER_EVENT_ROW | TRIGGER_EVENT_INSTEAD;
        let desc = Rc::clone(&self.desc);
        // bounded("catalog-sized: one per trigger of the view (its pg_trigger rows); a matching INSTEAD OF UPDATE trigger's function is called once for the row, as ExecIRUpdateTriggers loops them")
        for (i, trigger) in desc.triggers.iter().enumerate() {
            if trigger.tgtype & (TRIGGER_TYPE_LEVEL_MASK | TRIGGER_TYPE_TIMING_MASK | TRIGGER_TYPE_UPDATE) != TRIGGER_TYPE_ROW | TRIGGER_TYPE_INSTEAD | TRIGGER_TYPE_UPDATE || !TriggerEnabled(trigger) {
                continue;
            }
            let ViewRows { when, old, new, scratch, fmgr, .. } = self;
            let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: None };
            if !w.check(i, trigger, view, event, Some(old), Some(new))? {
                continue;
            }
            // trigger.c's should_free: a copied fetch owns its image and outlives the call
            let (nimg, nlen, ntid, ntoid, _owned) = match exectuples::exec_fetch_slot_heap_tuple(new, true, scratch.mcx(), rmcx)? {
                exectuples::FetchedHeapTuple::Slot(t) => (t.header_ptr(), t.t_len, t.t_self, t.t_tableOid, None),
                exectuples::FetchedHeapTuple::Copied(t) => (t.header_ptr(), t.t_len, t.t_self, t.t_tableOid, Some(t)),
            };
            // the OLD image is the wholerow cell's (live for the call); NEW is the slot's materialized image or a row-context
            // copy, neither written while the trigger runs
            let mut oldt = super::raw::tuple_at(img, len, tid, types_core::InvalidOid);
            let mut newt = super::raw::tuple_at(nimg, nlen, ntid, ntoid);
            let new_nn = NonNull::from(&mut newt);
            let mut tdata = TriggerData::from_raw(event, view, Some(NonNull::from(&mut oldt)), Some(new_nn), trigger);
            let finfo = fmgr.get(i, trigger.tgfoid)?;
            match ExecCallTriggerFunc(rmcx, &mut tdata, finfo, None)? {
                None => return Ok(false),
                Some(p) if p == new_nn => {}
                // ExecForceStoreHeapTuple(newtuple, newslot): the next trigger's NEW and the row RETURNING reads
                Some(p) => store_returned(super::raw::returned_tuple(p), new, rmcx, scratch.mcx())?,
            }
        }
        Ok(true)
    }

    /// trigger.c ExecIRDeleteTriggers: each matching trigger in order over OLD; a NULL return suppresses the delete.
    fn delete(&mut self, view: &Relation<'static>, img: *const u8, len: u32, tid: ItemPointerData) -> PgResult<bool> {
        let rmcx = self.scratch.exec_mcx();
        let event = TRIGGER_EVENT_DELETE | TRIGGER_EVENT_ROW | TRIGGER_EVENT_INSTEAD;
        let desc = Rc::clone(&self.desc);
        // bounded("catalog-sized: one per trigger of the view (its pg_trigger rows); a matching INSTEAD OF DELETE trigger's function is called once for the row, as ExecIRDeleteTriggers loops them")
        for (i, trigger) in desc.triggers.iter().enumerate() {
            if trigger.tgtype & (TRIGGER_TYPE_LEVEL_MASK | TRIGGER_TYPE_TIMING_MASK | TRIGGER_TYPE_DELETE) != TRIGGER_TYPE_ROW | TRIGGER_TYPE_INSTEAD | TRIGGER_TYPE_DELETE || !TriggerEnabled(trigger) {
                continue;
            }
            let ViewRows { when, old, scratch, fmgr, .. } = self;
            let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: None };
            if !w.check(i, trigger, view, event, Some(old), None)? {
                continue;
            }
            // as `update`'s OLD
            let mut oldt = super::raw::tuple_at(img, len, tid, types_core::InvalidOid);
            let mut tdata = TriggerData::from_raw(event, view, Some(NonNull::from(&mut oldt)), None, trigger);
            let finfo = fmgr.get(i, trigger.tgfoid)?;
            if ExecCallTriggerFunc(rmcx, &mut tdata, finfo, None)?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The last done row's cells for RETURNING (UPDATE: the final NEW; DELETE: OLD).
    pub fn written(&self) -> Option<(&[u64], &[u8])> {
        self.done.then(|| (&self.words[..], &self.wnulls[..]))
    }

    /// D667: RETURNING OLD of a done UPDATE: the view's old row (the wholerow junk, deformed in `old`); by-reference cells
    /// point into the slot's retained buffer until the next row starts.
    pub fn old_row(&mut self, values: &mut [u64], isnull: &mut [u8]) -> bool {
        if !self.done || self.cmd != Cmd::Update {
            return false;
        }
        exectuples::slot_getallattrs(&mut self.old);
        let base = self.old.base();
        let n = values.len().min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        true
    }

    /// H-f: a MERGE's other action wrote the statement's latest row: this host's last row is no longer RETURNING's.
    pub fn forget(&mut self) {
        self.done = false;
    }

    /// A new row: the last row's memory goes (RETURNING has read it by now).
    fn start_row(&mut self) {
        self.done = false;
        exectuples::exec_clear_tuple(&mut self.old, self.scratch.mcx());
        exectuples::exec_clear_tuple(&mut self.new, self.scratch.mcx());
        self.scratch.reset_exec();
    }
}

/// A trigger's returned tuple, copied into the row context and force-stored as NEW (ExecForceStoreHeapTuple).
fn store_returned(returned: &HeapTupleData<'_>, new: &mut SlotData<'static>, rmcx: Mcx<'static>, slot_mcx: Mcx<'static>) -> PgResult<()> {
    // the trigger's returned tuple, live in the per-call (row) context for `t_len` bytes
    let img = super::raw::tuple_bytes(returned);
    let mut buf = mcx::vec_with_capacity_in(rmcx, img.len())?;
    mcx::vec_append_bytes(&mut buf, img).map_err(|_| rmcx.oom(img.len()))?;
    let ptr = buf.as_ptr();
    core::mem::forget(buf);
    // a fresh row-context copy of the returned image
    let copy = super::raw::tuple_at(ptr, returned.t_len, returned.t_self, returned.t_tableOid);
    exectuples::exec_force_store_heap_tuple(copy, new, rmcx)?;
    exectuples::exec_materialize_slot(new, slot_mcx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_columns_are_the_updated_cols_members_as_zero_based_attributes() {
        let ctx = mcx::MemoryContext::new("t");
        let m = ctx.mcx();
        let mut u = Bitmapset::empty();
        for a in [2, 4] {
            u.add_member(m, a + ATTNO_OFFSET).unwrap();
        }
        assert_eq!(set_columns(&u, 4), vec![1, 3]); // alloc: Cold
        assert_eq!(set_columns(&u, 3), vec![1], "a member past the view's columns is not a column"); // alloc: Cold
        assert_eq!(set_columns(&Bitmapset::empty(), 4), Vec::<usize>::new());
    }
}
