use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, Locker, Outcome, PerTuple, Pins, Snapshot, WriteOp, Writer};
use gravity_hosts::{LockHost, WriteHost};
use gravity_program::{Cmd, IndexPlan, LockKind, WaitPolicy, WritePlan};
use mcx::MemoryContext;
use types_snapshot::SnapshotData;
use types_tuple::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

#[test]
fn lock_a_live_row_then_a_deleted_one() {
    let _g = serial();
    install();
    set_unique_index(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &[5, 6]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let mut l = LockHost::new(&WritePlan::locking(HEAP_OID, LockKind::Exclusive, WaitPolicy::Block));
    l.bind(&b).unwrap();
    assert_eq!(l.lock(pos(0)).unwrap(), Outcome::Done);
    assert_eq!(l.lock(pos(0)).unwrap(), Outcome::Done, "re-locking our own lock");
    l.unbind();
    let st = gravity_program::Arena::new(256);
    let mut del = WriteHost::new(&WritePlan::write(st.handle(), Cmd::Delete, HEAP_OID, &[IndexPlan::new(st.handle(), IDX_OID, &[1], false)]));
    del.bind(&b).unwrap();
    assert_eq!(del.write(WriteOp::Delete, pos(1), &[], &[]).unwrap(), Outcome::Done);
    del.unbind();
    for _ in 0..20 {
        l.bind(&b).unwrap();
        assert_eq!(l.lock(pos(1)).unwrap(), Outcome::Skipped, "a row deleted by this transaction is skipped");
        l.unbind();
    }
    drop(l);
    drop(del);
    quiesced();
}
