//! D774 (M4-CLOSE B3): the written row's system columns for RETURNING (`WritePlan.sys`), a sub-file of write.rs (no hosts
//! lib.rs line). PostgreSQL's ExecProcessReturning evaluates `ctid`, `xmin`, `cmin`, `xmax`, `cmax` over the slot the row
//! op left (`slot_getsysattr`) and `tableoid` as the result relation's (`tts_tableOid`, set there). The host keeps what that
//! slot would answer, per row op: the tuple an INSERT / UPDATE formed and heap_insert / heap_update stamped (its header,
//! its new `t_self`), or the version a DELETE fetched for RETURNING (a heap slot). A row PostgreSQL returns from a virtual
//! slot (a routed INSERT into a leaf of the root's own layout: ExecPrepareTupleRouting converts nothing, so the root's
//! virtual slot is inserted) has its `ctid` and `tableoid` only; its header fields are the "cannot retrieve a system column
//! in this context" error, raised when the plan reads them (`sys_header`), as ExecEvalSysVar raises it at the row.
//!
//! V4R-501 (`WritePlan.sys_old`): RETURNING OLD's, the replaced version's, into the old site's last six cells (`fill_old`)
//! from the version the host fetched after the row op (write.rs `fetch_version`: an UPDATE's, after an EvalPlanQual
//! recheck the newer version it locked; ON CONFLICT DO UPDATE's existing row; a moved row's deleted version), which is
//! PostgreSQL's oldSlot: `ctid` its position, xmin .. cmax its header as the page holds it after the row op (a buffer
//! tuple's `slot_getsysattr`: xmax / cmax the updater's), `tableoid` its relation.
use gravity_abi::Datum as Word;
use gravity_program::WritePlan;
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED};
use types_slot::SlotData;
use types_tuple::{HeapTupleData, ItemPointerData};

/// The system attnos in the sys site's cell order after `ctid`: xmin, cmin, xmax, cmax.
const HEADER: [i32; 4] = [-2, -3, -4, -5];

pub(crate) struct SysCols {
    /// The plan asks for them (`WritePlan.sys`); `header`: xmin .. cmax too.
    pub(crate) on: bool,
    header: bool,
    /// The last row op's: `ctid` lives here (its datum points at it) until the next row op.
    tid: Box<ItemPointerData>,
    words: [Word; 4],
    rel: Oid,
    /// A row was written since the last clear; `header_ok`: its header fields are readable (not a virtual slot's row).
    filled: bool,
    header_ok: bool,
    /// V4R-501: the old site carries the replaced version's six (`old_tid` its ctid's storage); `ins`: the last row op was
    /// an INSERT (under ON CONFLICT DO UPDATE its sites are one further from the sys site than the update's).
    old_on: bool,
    old_tid: Box<ItemPointerData>,
    ins: bool,
}

impl SysCols {
    pub(crate) fn new(plan: &WritePlan) -> SysCols {
        SysCols { on: plan.sys, header: plan.sys_header, tid: Box::default(), words: [0; 4], rel: 0, filled: false, header_ok: false, old_on: plan.sys_old, old_tid: Box::default(), ins: false } // alloc: Node
    }

    /// A routed leaf's host takes its route host's flags (write_route.rs).
    pub(crate) fn with(on: bool, header: bool) -> SysCols {
        SysCols { on, header, ..SysCols::new(&WritePlan::SELECT) }
    }

    pub(crate) fn header(&self) -> bool {
        self.header
    }

    pub(crate) fn clear(&mut self) {
        self.filled = false;
    }

    /// V4R-501: a target's host under a multi-target host fills the replaced version's too (write_route.rs).
    pub(crate) fn set_old(&mut self, on: bool) {
        self.old_on = on;
    }

    /// The row op this host runs (`Writer::write`): an INSERT's sys site is past ON CONFLICT DO UPDATE's conflict site.
    pub(crate) fn op(&mut self, insert: bool) {
        self.ins = insert;
    }

    /// After heap_insert / heap_update: the tuple they stamped (its `t_self` the new version's position).
    pub(crate) fn from_tuple(&mut self, tup: &HeapTupleData<'_>, rel: Oid) {
        if !self.on {
            return;
        }
        *self.tid = tup.t_self;
        let mut isnull = false;
        // bounded("const-domain: HEADER's four system columns")
        for (w, a) in self.words.iter_mut().zip(HEADER) {
            *w = types_tuple::getattr::heap_getsysattr(tup, a, &mut isnull).as_u64();
        }
        (self.rel, self.filled, self.header_ok) = (rel, true, true);
    }

    /// The row in `slot` (a DELETE's fetched version, an AM's inserted slot): `slot_getsysattr`, which raises for a
    /// slot without a tuple header as PostgreSQL's does.
    pub(crate) fn from_slot(&mut self, slot: &SlotData<'_>, rel: Oid) -> PgResult<()> {
        if !self.on {
            return Ok(());
        }
        *self.tid = slot.base().tts_tid;
        if self.header {
            let mut isnull = false;
            // bounded("const-domain: HEADER's four system columns")
            for (w, a) in self.words.iter_mut().zip(HEADER) {
                *w = exectuples::slot_getsysattr(slot, a, &mut isnull)?.as_u64();
            }
        }
        (self.rel, self.filled, self.header_ok) = (rel, true, true);
        Ok(())
    }

    /// The six cells (ctid, xmin, cmin, xmax, cmax, tableoid) of the last row op; `virtual_row`: PostgreSQL returned that
    /// row from a virtual slot (its header fields raise).
    pub(crate) fn fill(&self, values: &mut [Word], isnull: &mut [u8], virtual_row: bool) -> PgResult<bool> {
        if !self.on || !self.filled || values.len() < 6 || isnull.len() < 6 {
            return Ok(false);
        }
        if self.header && (virtual_row || !self.header_ok) {
            return Err(Box::new(PgError::error("cannot retrieve a system column in this context").with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED))); // alloc: Cold
        }
        values[0] = &*self.tid as *const ItemPointerData as usize as Word;
        values[1..5].copy_from_slice(&self.words);
        values[5] = self.rel as Word;
        isnull[..6].fill(0);
        Ok(true)
    }

    /// The sys site's offset from the site of the row op: after the old site when there is one (write/rowop_step.rs);
    /// V4R-501: an INSERT's from the write site, past DO UPDATE's conflict site (`conflict`).
    pub(crate) fn off(&self, old: bool, conflict: bool) -> usize {
        if self.on {
            1 + old as usize + (conflict && self.ins) as usize
        } else {
            0
        }
    }

    /// V4R-501: the old site's cells that are the row's (all but the six system columns' when the plan reads them).
    pub(crate) fn old_row(&self, n: usize) -> usize {
        if self.old_on {
            n.saturating_sub(6)
        } else {
            n
        }
    }

    /// V4R-501: the replaced version in `slot` (of relation `rel`) as the old site's last six cells: ctid, xmin, cmin,
    /// xmax, cmax, tableoid.
    pub(crate) fn fill_old(&mut self, slot: &SlotData<'_>, rel: Oid, values: &mut [Word], isnull: &mut [u8]) -> PgResult<()> {
        if !self.old_on {
            return Ok(());
        }
        if values.len() < 6 || isnull.len() != values.len() {
            // the lowerer lays the row's cells then these six (write/rowop_step.rs): never the previous row's left in them
            return Err(Box::new(PgError::error("gravity: Reason::Bug: V4R-501: an old site without its six system-column cells").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))); // alloc: Cold
        }
        let k = values.len() - 6;
        *self.old_tid = slot.base().tts_tid;
        values[k] = &*self.old_tid as *const ItemPointerData as usize as Word;
        let mut isnull_ = false;
        // bounded("const-domain: HEADER's four system columns")
        for (j, a) in HEADER.into_iter().enumerate() {
            values[k + 1 + j] = exectuples::slot_getsysattr(slot, a, &mut isnull_)?.as_u64();
        }
        values[k + 5] = rel as Word;
        isnull[k..].fill(0);
        Ok(())
    }
}
