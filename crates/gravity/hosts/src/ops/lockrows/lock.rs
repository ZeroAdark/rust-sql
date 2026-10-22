use std::rc::Weak;

use gravity_abi::Datum as Word;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, Locker, Outcome, Result};
use gravity_program::{LockKind, WaitPolicy, WritePlan};
use tableam_vocab::{LockTupleMode, LockWaitPolicy, TM_FailureData, TM_Result, TUPLE_LOCK_FLAG_FIND_LAST_VERSION, TUPLE_LOCK_FLAG_LOCK_UPDATE_IN_PROGRESS};
use types_core::{CommandId, Oid};
use types_error::{PgError, PgResult, ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION};
use types_rel::{Relation, RelationData};
use types_slot::SlotData;

use crate::ops::shared::rel::{open_heap, park, pos_of, tid_of};
use crate::scratch::NodeScratch;

#[derive(Clone, Copy, Debug)]
pub struct LockRecipe {
    pub heap: Oid,
    pub mode: LockTupleMode,
    pub policy: LockWaitPolicy,
}

impl LockRecipe {
    pub fn from_plan(p: &WritePlan) -> LockRecipe {
        let mode = match p.lock {
            LockKind::KeyShare => LockTupleMode::LockTupleKeyShare,
            LockKind::Share => LockTupleMode::LockTupleShare,
            LockKind::NoKeyExclusive => LockTupleMode::LockTupleNoKeyExclusive,
            LockKind::Exclusive | LockKind::None => LockTupleMode::LockTupleExclusive,
        };
        let policy = match p.wait {
            WaitPolicy::Block => LockWaitPolicy::LockWaitBlock,
            WaitPolicy::Skip => LockWaitPolicy::LockWaitSkip,
            WaitPolicy::Error => LockWaitPolicy::LockWaitError,
        };
        LockRecipe { heap: p.target_rel, mode, policy }
    }
}

/// One locked relation: the recipe's heap first, then (H-h) each leaf a row's position names (a partitioned or inherited
/// mark's children, ExecLockRows' per-child ExecRowMark), opened at its first row and parked with the rest at unbind.
struct Rel {
    oid: Oid,
    heap: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    slot: Option<SlotData<'static>>,
    /// lockrows REFFETCH: the slot `fetch` fills (its own, so a fetch from the locked relation keeps `existing`'s version).
    fetched: Option<SlotData<'static>>,
}

pub struct LockHost {
    recipe: LockRecipe,
    rels: Vec<Rel>,
    /// The relation of the last lock (its slot holds the traversed version `existing` reads).
    last: usize,
    cid: CommandId,
    recheck_pos: ScanPos,
    /// C5b: ExecLockRows' erm->curCtid: the tid the last lock left (the newest version when it followed the update chain,
    /// TUPLE_LOCK_FLAG_FIND_LAST_VERSION's in/out tid) in its leaf relation; WHERE CURRENT OF reads it (entry current_of.rs).
    current: ScanPos,
    /// D604: the node runs as an EPQ recheck (`Locker::epq`): every lock answers `Done` without locking.
    epq: bool,
    bound: bool,
    scratch: NodeScratch,
}

impl LockHost {
    pub fn new(plan: &WritePlan) -> LockHost {
        let rels = vec![Rel { oid: plan.target_rel, heap: None, parked: None, slot: None, fetched: None }]; // alloc: Node
        LockHost { recipe: LockRecipe::from_plan(plan), rels, last: 0, cid: 0, recheck_pos: ScanPos::NONE, current: ScanPos::NONE, epq: false, bound: false, scratch: NodeScratch::new("GravityLockHost") }
    }

    fn bind_pg(&mut self, cid: CommandId) -> PgResult<()> {
        self.open(0)?;
        self.cid = cid;
        self.last = 0;
        self.recheck_pos = ScanPos::NONE;
        self.bound = true;
        Ok(())
    }

    /// Relation `i` opened for this bind (its lock is the plan's: the range table's, as ExecGetRangeTableRelation's NoLock).
    fn open(&mut self, i: usize) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        let r = &mut self.rels[i];
        if r.heap.is_none() {
            let heap = open_heap(mcx, &r.parked, r.oid)?;
            if r.slot.is_none() {
                r.slot = Some(tableam::table_slot_create(mcx, &heap)?);
            }
            r.heap = Some(heap);
        }
        Ok(())
    }

    /// H-h: the relation a row's position names (a leaf of the mark: ExecLockRows locks each child's row in its own
    /// relation); a position without one, or naming the recipe's heap, is the heap's.
    fn rel_of(&mut self, pos: ScanPos) -> PgResult<usize> {
        if pos.rel == 0 || pos.rel == self.recipe.heap {
            return Ok(0);
        }
        let i = match self.rels.iter().position(|r| r.oid == pos.rel) {
            Some(i) => i,
            None => {
                self.rels.push(Rel { oid: pos.rel, heap: None, parked: None, slot: None, fetched: None }); // alloc: Node
                self.rels.len() - 1
            }
        };
        self.open(i)?;
        Ok(i)
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        if !self.bound {
            return Ok(());
        }
        self.bound = false;
        self.epq = false;
        // bounded("catalog-sized: one relation of the mark each: its heap and the partition leaves its locked rows named, at most the leaves of the mark's partition tree")
        for r in self.rels.iter_mut() {
            for slot in [r.slot.as_mut(), r.fetched.as_mut()].into_iter().flatten() {
                exectuples::exec_clear_tuple(slot, self.scratch.mcx());
            }
            if r.heap.is_some() {
                r.parked = park(r.heap.take())?;
            }
        }
        self.scratch.reset_exec();
        Ok(())
    }

    /// ExecLockRows' table_tuple_lock: under read committed the lock follows
    /// the update chain to the newest committed version (`traversed`), which
    /// becomes the EPQ test tuple (`Recheck`, position in `recheck_pos`);
    /// under a transaction snapshot a concurrent update is a serialization
    /// failure. TM_Deleted / skip-locked rows are skipped.
    fn lock_pg(&mut self, pos: ScanPos) -> PgResult<Outcome> {
        if let Some(slot) = self.rels[self.last].slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        self.recheck_pos = ScanPos::NONE;
        let i = self.rel_of(pos)?;
        self.last = i;
        let Rel { oid, heap, slot, .. } = &mut self.rels[i];
        let (oid, heap) = (*oid, heap.as_ref().expect("gravity: lock on an unbound locker"));
        let slot = slot.as_mut().expect("gravity: lock on an unbound locker");
        let tid = tid_of(pos);
        let mut tmfd = TM_FailureData::default();
        let xact_snapshot = crate::xact_snapshot();
        let flags = TUPLE_LOCK_FLAG_LOCK_UPDATE_IN_PROGRESS | if xact_snapshot { 0 } else { TUPLE_LOCK_FLAG_FIND_LAST_VERSION };
        let r = tableam::table_tuple_lock(self.scratch.exec_mcx(), heap, &tid, &None, slot, self.cid, self.recipe.mode, self.recipe.policy, flags, &mut tmfd);
        let r = r?;
        let out = match r {
            TM_Result::TM_Ok if tmfd.traversed => {
                self.recheck_pos = pos_of(oid, &slot.base().tts_tid);
                self.current = self.recheck_pos;
                // RL1b: the newest version stays in the slot (its buffer pinned) for `existing`, until the next lock or unbind
                return Ok(Outcome::Recheck);
            }
            TM_Result::TM_Ok => {
                self.current = pos_of(oid, &tid);
                Ok(Outcome::Done)
            }
            // ExecLockRows: a row this command or a later one of the transaction updated is skipped (the Halloween rule)
            TM_Result::TM_SelfModified => Ok(Outcome::Skipped),
            TM_Result::TM_Updated | TM_Result::TM_Deleted if xact_snapshot => Err(crate::serialization_failure()),
            // ExecLockRows' elogs print the status number (%u)
            TM_Result::TM_Updated => Err(Box::new(PgError::error(format!("unexpected table_tuple_lock status: {}", TM_Result::TM_Updated as u32)))), // alloc: Cold
            TM_Result::TM_Deleted | TM_Result::TM_WouldBlock => Ok(Outcome::Skipped),
            TM_Result::TM_Invisible => Err(Box::new(PgError::error("attempted to lock invisible tuple".to_string()))), // alloc: Cold
            other => Err(Box::new(PgError::error(format!("unrecognized table_tuple_lock status: {}", other as u32)))), // alloc: Cold
        };
        exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        out
    }

    /// lockrows REFFETCH: EvalPlanQualFetchRowMark for a REFERENCE mark of an ordinary table: the version at `pos` (the row's
    /// junk ctid in the mark's relation) under SnapshotAny (table_tuple_fetch_row_version), into the relation's fetch slot,
    /// whose buffer stays pinned until the next fetch or unbind.
    fn fetch_pg(&mut self, pos: ScanPos, values: &mut [Word], isnull: &mut [u8]) -> PgResult<ScanPos> {
        let i = self.rel_of(pos)?;
        let mcx = self.scratch.mcx();
        let Rel { oid, heap, fetched, .. } = &mut self.rels[i];
        let heap = heap.as_ref().expect("gravity: fetch on an unbound locker");
        if fetched.is_none() {
            *fetched = Some(tableam::table_slot_create(mcx, heap)?);
        }
        let slot = fetched.as_mut().expect("gravity: the fetch slot");
        exectuples::exec_clear_tuple(slot, mcx);
        if !tableam::table_tuple_fetch_row_version(self.scratch.exec_mcx(), heap, &tid_of(pos), &None, slot)? {
            return Err(Box::new(PgError::error("failed to fetch tuple for EvalPlanQual recheck".to_string()))); // alloc: Cold
        }
        exectuples::slot_getallattrs(slot);
        let base = slot.base();
        let n = values.len().min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the fetched row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        Ok(pos_of(*oid, &base.tts_tid))
    }
}

impl Locker for LockHost {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.bind_pg(b.command_id.0).map_err(crate::stash)
    }

    fn lock(&mut self, pos: ScanPos) -> Result<Outcome> {
        if self.epq {
            return Ok(Outcome::Done);
        }
        // D2947: a KNN walk's reordered row (its invalid TID, ctid (4294967295,0)): ExecLockRows locks that TID and
        // table_tuple_lock answers TM_Invisible
        if crate::ops::shared::rel::invalid_tid(pos) {
            return Err(crate::stash(Box::new(PgError::error("attempted to lock invisible tuple".to_string())))); // alloc: Cold
        }
        // lockrows D2604: an inactive family child's mark (the row's tableoid names another leaf; lowering stores the empty
        // position): ExecLockRows' ermActive false, curCtid invalid, nothing locked, the next mark tried
        if pos.off == 0 {
            self.current = ScanPos::NONE;
            return Ok(Outcome::Done);
        }
        self.lock_pg(pos).map_err(crate::stash)
    }

    fn epq(&mut self, on: bool) {
        self.epq = on;
    }

    fn recheck_pos(&self) -> ScanPos {
        self.recheck_pos
    }

    fn current(&self) -> ScanPos {
        self.current
    }

    /// RL1b: the version the last lock traversed to (ExecLockRows' EPQ test tuple, `EvalPlanQualSlot`), its cells as the
    /// lock site's; read from the slot the lock filled, never locked again (D604). By-reference cells point into the
    /// pinned buffer until the next lock or unbind.
    fn existing(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        if self.recheck_pos == ScanPos::NONE {
            return Err(gravity_driver::Error::host("gravity: a lock's cells without a traversed version"));
        }
        let r = &mut self.rels[self.last];
        let slot = r.slot.as_mut().expect("gravity: existing on an unbound locker");
        exectuples::slot_getallattrs(slot);
        let base = slot.base();
        let n = values.len().min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the fetched row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        Ok(pos_of(r.oid, &base.tts_tid))
    }

    /// lockrows REFFETCH (`fetch_pg`): an in-IR recheck's REFERENCE-marked row, fetched without a lock (in a recheck node too).
    fn fetch(&mut self, pos: ScanPos, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        self.fetch_pg(pos, values, isnull).map_err(crate::stash)
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: lock host unbind");
    }
}

impl Drop for LockHost {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        // bounded("catalog-sized: one relation of the mark each: its heap and the partition leaves its locked rows named, at most the leaves of the mark's partition tree")
        for r in self.rels.iter_mut() {
            r.slot = None;
            r.fetched = None;
        }
    }
}
