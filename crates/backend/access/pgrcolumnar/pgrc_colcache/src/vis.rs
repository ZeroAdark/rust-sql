//! `HeapTupleSatisfiesMVCC` over a header copy, with no buffer and no hint-bit writes.
//! No combo cids arise on a companion, so `cid` is cmin, or cmax for a row another transaction inserted.

use types_core::xact::TransactionIdIsValid;
use types_core::{CommandId, TransactionId};
use types_error::PgResult;
use types_snapshot::SnapshotData;
use types_tuple::{HEAP_COMBOCID, HEAP_MOVED, HEAP_XMAX_COMMITTED, HEAP_XMAX_INVALID, HEAP_XMAX_IS_LOCKED_ONLY, HEAP_XMAX_IS_MULTI, HEAP_XMIN_COMMITTED, HEAP_XMIN_FROZEN, HEAP_XMIN_INVALID};

use crate::block::Hdr;

pub trait SnapView {
    fn is_current(&self, xid: TransactionId) -> bool;
    fn in_snapshot(&self, xid: TransactionId) -> PgResult<bool>;
    fn did_commit(&self, xid: TransactionId) -> PgResult<bool>;
    fn curcid(&self) -> CommandId;
}

pub struct Mvcc<'a, 'mcx> {
    pub snap: &'a SnapshotData<'mcx>,
}

impl SnapView for Mvcc<'_, '_> {
    fn is_current(&self, xid: TransactionId) -> bool {
        xact::TransactionIdIsCurrentTransactionId(xid)
    }

    fn in_snapshot(&self, xid: TransactionId) -> PgResult<bool> {
        snapmgr::XidInMVCCSnapshot(xid, self.snap)
    }

    fn did_commit(&self, xid: TransactionId) -> PgResult<bool> {
        transam_seams::transaction_id_did_commit::call(xid)
    }

    fn curcid(&self) -> CommandId {
        self.snap.curcid.get()
    }
}

/// `Some(v)` decides the row; `None` = inserted and visible so far, go on to xmax.
fn xmin_half(h: &Hdr, s: &dyn SnapView) -> PgResult<Option<bool>> {
    debug_assert!(h.infomask & HEAP_COMBOCID == 0, "combo cid on a companion row");
    debug_assert!(h.infomask & HEAP_MOVED == 0, "VACUUM FULL moved a companion row");
    if h.infomask & HEAP_XMIN_COMMITTED == 0 {
        if h.infomask & HEAP_XMIN_INVALID != 0 || !TransactionIdIsValid(h.xmin) {
            return Ok(Some(false));
        }
        if s.is_current(h.xmin) {
            if h.cid >= s.curcid() {
                return Ok(Some(false));
            }
            return Ok(None);
        }
        if s.in_snapshot(h.xmin)? {
            return Ok(Some(false));
        }
        if !s.did_commit(h.xmin)? {
            return Ok(Some(false));
        }
    } else if h.infomask & HEAP_XMIN_FROZEN != HEAP_XMIN_FROZEN && s.in_snapshot(h.xmin)? {
        return Ok(Some(false));
    }
    Ok(None)
}

/// A segment row (never deleted).
pub fn seg_visible(h: &Hdr, s: &dyn SnapView) -> PgResult<bool> {
    Ok(xmin_half(h, s)?.unwrap_or(true))
}

pub fn tomb_visible(h: &Hdr, s: &dyn SnapView) -> PgResult<bool> {
    let current_xmin = h.infomask & HEAP_XMIN_COMMITTED == 0 && s.is_current(h.xmin);
    if let Some(v) = xmin_half(h, s)? {
        return Ok(v);
    }
    if h.infomask & HEAP_XMAX_INVALID != 0 || !TransactionIdIsValid(h.xmax) || HEAP_XMAX_IS_LOCKED_ONLY(h.infomask) {
        return Ok(true);
    }
    debug_assert!(h.infomask & HEAP_XMAX_IS_MULTI == 0, "block decode refuses multixact tombstones");
    if current_xmin {
        // Inserted and deleted by this transaction needs a combo cid: never on a companion.
        return Ok(!s.is_current(h.xmax));
    }
    if h.infomask & HEAP_XMAX_COMMITTED == 0 {
        if s.is_current(h.xmax) {
            return Ok(h.cid >= s.curcid());
        }
        if s.in_snapshot(h.xmax)? {
            return Ok(true);
        }
        return Ok(!s.did_commit(h.xmax)?);
    }
    Ok(s.in_snapshot(h.xmax)?)
}
