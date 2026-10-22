use std::rc::Rc;

use datum::Datum;
use gravity_abi::ScanPos;
use gravity_program::Cmd;
use mcx::Mcx;
use tableam_vocab::{LockTupleMode, LockWaitPolicy, TM_FailureData, TM_Result, TUPLE_LOCK_FLAG_FIND_LAST_VERSION};
use trigger::{TriggerFmgrCache, TriggerWhenCache, TriggerWhenEval};
use types_core::CommandId;
use types_error::{PgError, PgResult, ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION, ERRCODE_T_R_SERIALIZATION_FAILURE};
use types_rel::Relation;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_trigger::TriggerDesc;
use types_tuple::ItemPointerData;

use crate::ops::shared::rel::pos_of;
use crate::scratch::NodeScratch;
use crate::ops::modifytable::write_constraints::ModifiedCols;

/// What the BEFORE ROW prologue decided for the row: the write goes on (its cells possibly the trigger's), the row is
/// skipped (a trigger returned NULL, or the locked row is gone), the old row moved under read committed and the
/// runner re-projects over its newest version (`GetTupleForTrigger`'s EvalPlanQual leg), or (D2042) a MERGE action's
/// locked row was deleted concurrently, so the source row is no longer matched (ExecMergeMatched's TM_Deleted after
/// ExecUpdatePrologue / ExecDeletePrologue: `*matched = false`, the NOT MATCHED actions run for it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pre {
    Go,
    Skip,
    Recheck(ScanPos),
    NotMatched,
}

pub struct BeforeRow {
    desc: Rc<TriggerDesc<'static>>,
    fmgr: TriggerFmgrCache,
    when: TriggerWhenCache<'static>,
    modified: Option<ModifiedCols>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    old: SlotData<'static>,
    new: SlotData<'static>,
    /// D822: ON CONFLICT DO UPDATE's EXCLUDED, the BEFORE INSERT trigger's returned row of this same row, moved out of
    /// `new` when the arm's BEFORE UPDATE fires (`update(.., conflict)`): the arm's SET values point into it, so it
    /// lives until the next row begins, as ExecOnConflictUpdate's excluded slot does.
    excluded: SlotData<'static>,
    words: Vec<u64>,
    wnulls: Vec<u8>,
    replaced: bool,
    /// The row's cells are a trigger's (`words`), alive in `scratch`'s exec region until the next row begins
    /// (`begin_row`): the kernel's RETURNING projects over them after the write (`Writer::written`).
    returned: bool,
    /// D725: a BEFORE UPDATE's lock mode (ExecUpdateLockMode over `modified`, the leaf's numbering), per bind.
    update_mode: Option<LockTupleMode>,
    /// V4R-313: a MERGE action's prologue (ExecBRUpdateTriggers / ExecBRDeleteTriggers with is_merge_update /
    /// is_merge_delete): the lock's TM_SelfModified is ExecMergeMatched's error, not a skip.
    merge: bool,
    scratch: NodeScratch,
}

/// `GetTupleForTrigger`'s TM_SelfModified error (trigger.c): the row was modified by a later command of this
/// transaction before the BEFORE ROW lock. Its wording is "to be updated" for every event — UPDATE, DELETE and
/// MERGE alike (the "to be deleted" form is ExecDelete's, raised only after the triggers ran; `tm_error`).
fn data_change() -> Box<PgError> {
    Box::new(PgError::error("tuple to be updated was already modified by an operation triggered by the current command".to_string()).with_sqlstate(ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION).with_hint("Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.")) // alloc: Cold
}

impl BeforeRow {
    pub fn bind(heap: &Relation<'static>, desc: &Rc<TriggerDesc<'static>>, modified: Option<ModifiedCols>, snapshot: Option<Rc<SnapshotData<'static>>>) -> PgResult<BeforeRow> {
        let scratch = NodeScratch::new("GravityTriggerBefore");
        let old = tableam::table_slot_create(scratch.mcx(), heap)?;
        let new = tableam::table_slot_create(scratch.mcx(), heap)?;
        let excluded = tableam::table_slot_create(scratch.mcx(), heap)?;
        Ok(BeforeRow { desc: Rc::clone(desc), fmgr: TriggerFmgrCache::default(), when: TriggerWhenCache::default(), modified, snapshot, old, new, excluded, words: Vec::new(), wnulls: Vec::new(), replaced: false, returned: false, update_mode: None, merge: false, scratch }) // alloc: Node
    }

    pub fn merge(mut self, on: bool) -> BeforeRow {
        self.merge = on;
        self
    }

    /// The row's cells as the kernel's words when a trigger replaced them (the index entries and the ON CONFLICT
    /// arbiters read the written row): taken for the row, handed back by `keep` (the vectors' capacity is reused).
    pub fn take_words(&mut self) -> Option<(Vec<u64>, Vec<u8>)> {
        self.replaced.then(|| (core::mem::take(&mut self.words), core::mem::take(&mut self.wnulls)))
    }

    pub fn keep(&mut self, w: (Vec<u64>, Vec<u8>)) {
        self.words = w.0;
        self.wnulls = w.1;
    }

    /// Row end: the locked old row released; `new` (the trigger's returned tuple, the exec region) waits for `begin_row`.
    pub fn clear(&mut self) {
        let mcx = self.scratch.mcx();
        exectuples::exec_clear_tuple(&mut self.old, mcx);
        self.replaced = false;
    }

    /// Row start: the previous row's returned tuple and its region go; the trigger call and its returned tuple
    /// (`exec_force_store_heap_tuple`'s copy) land in the exec region, alive until the next `begin_row`.
    fn begin_row(&mut self) -> Mcx<'static> {
        exectuples::exec_clear_tuple(&mut self.new, self.scratch.mcx());
        exectuples::exec_clear_tuple(&mut self.excluded, self.scratch.mcx());
        self.scratch.reset_exec();
        self.returned = false;
        self.scratch.exec_mcx()
    }

    /// D822: the same row goes on (ON CONFLICT DO UPDATE after the INSERT's BEFORE ROW trigger): `new` moves to `excluded`
    /// (an empty slot takes its place), nothing freed.
    fn continue_row(&mut self) -> Mcx<'static> {
        core::mem::swap(&mut self.new, &mut self.excluded);
        self.returned = false;
        self.scratch.exec_mcx()
    }

    /// The trigger's returned row of the row just written (`take_new`), the kernel's words.
    pub fn returned(&self) -> Option<(&[u64], &[u8])> {
        self.returned.then(|| (&self.words[..], &self.wnulls[..]))
    }

    fn store_new(&mut self, heap: &Relation<'static>, values: &[Datum], nulls: &[bool]) {
        exectuples::exec_clear_tuple(&mut self.new, self.scratch.mcx());
        let n = values.len();
        let base = self.new.base_mut();
        base.tts_values[..n].copy_from_slice(values);
        base.tts_isnull[..n].copy_from_slice(nulls);
        exectuples::exec_store_virtual_tuple(&mut self.new);
        self.new.base_mut().tts_tableOid = heap.rd_id;
    }

    /// The trigger-returned row back into the caller's cells (`ExecForceStoreHeapTuple` left it in `new`): by-reference
    /// cells point into the slot's image, alive until `clear`.
    fn take_new(&mut self, values: &mut [Datum], nulls: &mut [bool]) {
        exectuples::slot_getallattrs(&mut self.new);
        let base = self.new.base();
        let n = values.len().min(base.tts_nvalid as usize);
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
        self.returned = true;
    }

    /// `GetTupleForTrigger`: the old row locked in `mode` into `old` under the statement's snapshot and command id.
    fn lock_old(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, otid: &ItemPointerData, cid: CommandId, mode: LockTupleMode) -> PgResult<Pre> {
        let xact_snapshot = crate::xact_snapshot();
        let flags = if xact_snapshot { 0 } else { TUPLE_LOCK_FLAG_FIND_LAST_VERSION };
        let mut tmfd = TM_FailureData::default();
        exectuples::exec_clear_tuple(&mut self.old, self.scratch.mcx());
        let r = tableam::table_tuple_lock(mcx, heap, otid, &self.snapshot, &mut self.old, cid, mode, LockWaitPolicy::LockWaitBlock, flags, &mut tmfd)?;
        match r {
            TM_Result::TM_Ok if tmfd.traversed => Ok(Pre::Recheck(pos_of(heap.rd_id, &self.old.base().tts_tid))),
            TM_Result::TM_Ok => Ok(Pre::Go),
            TM_Result::TM_SelfModified if tmfd.cmax != cid => Err(data_change()),
            // ExecMergeMatched's TM_SelfModified (the lock returned it un-rechecked): 21000 when this transaction's
            // command already touched the row
            TM_Result::TM_SelfModified if self.merge => Err(crate::ops::modifytable::write::merge_self_modified(&tmfd, cid)),
            TM_Result::TM_SelfModified => Ok(Pre::Skip),
            TM_Result::TM_Updated if xact_snapshot => Err(crate::serialization_failure()),
            TM_Result::TM_Deleted if xact_snapshot => Err(Box::new(PgError::error("could not serialize access due to concurrent delete".to_string()).with_sqlstate(ERRCODE_T_R_SERIALIZATION_FAILURE))), // alloc: Cold
            // D2042: GetTupleForTrigger hands TM_Deleted to ExecMergeMatched, which takes the row as no longer matched
            TM_Result::TM_Deleted if self.merge => Ok(Pre::NotMatched),
            TM_Result::TM_Deleted => Ok(Pre::Skip),
            TM_Result::TM_Invisible => Err(Box::new(PgError::error("attempted to lock invisible tuple".to_string()))), // alloc: Cold
            other => Err(Box::new(PgError::error(format!("unexpected table_tuple_lock status: {other:?}")))), // alloc: Cold
        }
    }

    pub fn insert(&mut self, _mcx: Mcx<'static>, heap: &Relation<'static>, values: &mut [Datum], nulls: &mut [bool]) -> PgResult<bool> {
        let mcx = self.begin_row();
        if !self.desc.trig_insert_before_row {
            return Ok(true);
        }
        self.store_new(heap, values, nulls);
        let desc = Rc::clone(&self.desc);
        let BeforeRow { fmgr, when, new, scratch, .. } = self;
        let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: None };
        if !trigger::ExecBRInsertTriggers(mcx, heap, &desc, fmgr, &mut w, new)? {
            return Ok(false);
        }
        self.take_new(values, nulls);
        Ok(true)
    }

    /// `conflict`: ON CONFLICT DO UPDATE's update of the row whose INSERT just conflicted. The row goes on (D822): the
    /// INSERT trigger's returned row (EXCLUDED, which `values` may point into) moves to `excluded` instead of being
    /// cleared, and the exec region is not reset, so both live until the next row's `begin_row`.
    pub fn update(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, otid: &ItemPointerData, cid: CommandId, values: &mut [Datum], nulls: &mut [bool], conflict: bool) -> PgResult<Pre> {
        let row_mcx = if conflict { self.continue_row() } else { self.begin_row() };
        if !self.desc.trig_update_before_row {
            return Ok(Pre::Go);
        }
        let mode = self.update_mode(heap)?;
        let pre = self.lock_old(mcx, heap, otid, cid, mode)?;
        if pre != Pre::Go {
            return Ok(pre);
        }
        self.store_new(heap, values, nulls);
        let desc = Rc::clone(&self.desc);
        let modified = self.modified.map(|m| m.1);
        let BeforeRow { fmgr, when, old, new, scratch, .. } = self;
        let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: modified };
        if !trigger::ExecBRUpdateTriggers(row_mcx, heap, &desc, fmgr, &mut w, old, new, modified)? {
            return Ok(Pre::Skip);
        }
        self.take_new(values, nulls);
        Ok(Pre::Go)
    }

    /// D725: ExecBRUpdateTriggers' `ExecUpdateLockMode`: exclusive when an updated column (`modified`: the statement's
    /// ExecGetAllUpdatedCols in this relation's numbering, trigger.rs) is a key column, else no-key exclusive; exclusive
    /// when the write carries no column list (the mode before D725).
    fn update_mode(&mut self, heap: &Relation<'static>) -> PgResult<LockTupleMode> {
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        if let Some(m) = self.update_mode {
            return Ok(m);
        }
        let key = match self.modified {
            Some((_, u)) => relcache_seams::relation_get_index_attr_bitmap::call(heap.rd_id)?.key.iter().any(|&a| u.is_member(a as i32 - FLIHAN)),
            None => true,
        };
        let m = if key { LockTupleMode::LockTupleExclusive } else { LockTupleMode::LockTupleNoKeyExclusive };
        self.update_mode = Some(m);
        Ok(m)
    }

    pub fn delete(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, otid: &ItemPointerData, cid: CommandId) -> PgResult<Pre> {
        self.begin_row();
        if !self.desc.trig_delete_before_row {
            return Ok(Pre::Go);
        }
        let pre = self.lock_old(mcx, heap, otid, cid, LockTupleMode::LockTupleExclusive)?;
        if pre != Pre::Go {
            return Ok(pre);
        }
        let desc = Rc::clone(&self.desc);
        let BeforeRow { fmgr, when, old, scratch, .. } = self;
        let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: None };
        if !trigger::ExecBRDeleteTriggers(mcx, heap, &desc, fmgr, &mut w, old)? {
            return Ok(Pre::Skip);
        }
        Ok(Pre::Go)
    }
}

/// The BEFORE ROW kinds a command fires (`cmd_bits` over the descriptor's before-row flags).
pub fn fires(desc: &TriggerDesc<'_>, cmd: Cmd) -> bool {
    match cmd {
        Cmd::Insert => desc.trig_insert_before_row,
        Cmd::Update => desc.trig_update_before_row,
        Cmd::Delete => desc.trig_delete_before_row,
        Cmd::Merge => desc.trig_insert_before_row || desc.trig_update_before_row || desc.trig_delete_before_row,
        Cmd::Select => false,
    }
}
