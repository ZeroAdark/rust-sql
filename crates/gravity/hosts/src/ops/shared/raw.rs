#![allow(unsafe_code)]
use std::rc::{Rc, Weak};

use gravity_driver::BindingRef;
use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_rel::{Relation, RelationData, LOCKMODE};
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;

use super::rel::repin;

pub fn open_heap(mcx: Mcx<'static>, parked: &Option<Weak<RelationData<'static>>>, oid: Oid) -> PgResult<Relation<'static>> {
    match repin(parked) {
        Some(h) => Ok(h),
        None => table::table_open(mcx, oid, NoLock),
    }
}

pub fn open_index(mcx: Mcx<'static>, parked: &Option<Weak<RelationData<'static>>>, oid: Oid, lockmode: LOCKMODE) -> PgResult<Relation<'static>> {
    if lockmode != NoLock {
        lmgr_seams::lock_relation_oid::call(oid, lockmode)?;
    }
    match repin(parked) {
        Some(i) => Ok(i),
        None => indexam::index_open(mcx, oid, NoLock),
    }
}

/// The leader's registered snapshot, by pointer (one registration per statement at the
/// leader, every ctx and worker reading that `SnapshotData` for the statement's duration — it is
/// immutable while the statement runs, the leader's registration keeps xmin protected, and a
/// worker never touches the `Rc`'s count nor unregisters it). Valid from bind to unbind.
pub fn snapshot_ptr(b: &BindingRef<'_>) -> *const SnapshotData<'static> {
    // SAFETY: Binding.snapshot is the address of the exec's Rc<SnapshotData>, alive for the statement.
    Rc::as_ptr(unsafe { &*(b.snapshot.0 as *const Rc<SnapshotData<'static>>) })
}

pub fn snapshot_of(b: &BindingRef<'_>) -> Rc<SnapshotData<'static>> {
    // SAFETY: Binding.snapshot is the address of the exec's Rc<SnapshotData>,
    // alive for the whole execution (gravity_exec::glue::binding_parts).
    unsafe { &*(b.snapshot.0 as *const Rc<SnapshotData<'static>>) }.clone()
}
