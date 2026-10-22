use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, Outcome, PerTuple, Pins, Snapshot, Source, WriteOp, Writer};
use gravity_hosts::{take_error, IndexSource, SiteRecipe, WriteHost};
use gravity_program::{Cmd, IndexPlan, WritePlan};
use mcx::MemoryContext;
use types_error::ERRCODE_UNIQUE_VIOLATION;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn source(mcx: mcx::Mcx<'_>) -> IndexSource {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, true), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    IndexSource::new(SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap())
}

fn find(src: &mut IndexSource, key: i32) -> Option<(i32, ScanPos)> {
    src.seek(&[key as u64], &[0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    let found = src.next(&mut v, &mut n).unwrap().then(|| (v[0] as i32, src.position()));
    while src.next(&mut v, &mut n).unwrap() {}
    found
}

fn plan(cmd: Cmd) -> WritePlan {
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    WritePlan::write(a, cmd, HEAP_OID, &[IndexPlan::new(a, IDX_OID, &[1], true)])
}

#[test]
fn insert_update_delete_round_trip_with_index_maintenance_and_duplicate_key_text() {
    let _g = serial();
    install();
    set_unique_index(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[10, 20]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut ins = WriteHost::new(&plan(Cmd::Insert));
    ins.bind(&b).unwrap();
    assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[30], &[0]).unwrap(), Outcome::Done);
    let dup = ins.write(WriteOp::Insert, ScanPos::NONE, &[20], &[0]).expect_err("duplicate key");
    let e = take_error().expect("stashed pg error");
    assert!(matches!(dup, gravity_driver::Error::Host(_)));
    assert_eq!(e.sqlstate(), ERRCODE_UNIQUE_VIOLATION);
    assert_eq!(e.message, "duplicate key value violates unique constraint \"t_idx\"");
    ins.unbind();

    let mut src = source(mcx);
    src.bind(&b).unwrap();
    let (v, pos) = find(&mut src, 30).expect("the inserted row is indexed");
    assert_eq!(v, 30);
    assert!(pos.is_valid() && pos.rel == HEAP_OID);
    src.unbind();

    let mut upd = WriteHost::new(&plan(Cmd::Update));
    upd.bind(&b).unwrap();
    assert_eq!(upd.write(WriteOp::Update, pos, &[31], &[0]).unwrap(), Outcome::Done);
    upd.unbind();
    src.bind(&b).unwrap();
    let (_, pos31) = find(&mut src, 31).expect("the new key is indexed");
    assert_ne!(pos31, pos);
    src.unbind();

    let mut del = WriteHost::new(&plan(Cmd::Delete));
    del.bind(&b).unwrap();
    assert_eq!(del.write(WriteOp::Delete, pos31, &[], &[]).unwrap(), Outcome::Done);
    assert_eq!(del.write(WriteOp::Delete, pos31, &[], &[]).unwrap(), Outcome::Skipped, "a second delete of the same version finds it gone");
    del.unbind();
    let used = del.scratch_used();
    for _ in 0..50 {
        ins.bind(&b).unwrap();
        ins.unbind();
    }
    assert_eq!(del.scratch_used(), used);
    let st = gravity_program::Arena::new(256);
    let mut w = WriteHost::new(&WritePlan::write(st.handle(), Cmd::Insert, HEAP_OID, &[IndexPlan::new(st.handle(), IDX_OID, &[1], false)]));
    // D613: the indexes open at the first row (ExecOpenIndices). D765: an index whose form is no longer the plan's recipe
    // (here: the plan says non-unique, the index is unique) is maintained as it is by the executor's own path, as PG does
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Insert, ScanPos::NONE, &[41], &[0]).unwrap(), Outcome::Done, "the index as it is takes the row");
    w.write(WriteOp::Insert, ScanPos::NONE, &[41], &[0]).expect_err("the unique index checks the duplicate");
    assert_eq!(take_error().expect("stashed pg error").sqlstate(), ERRCODE_UNIQUE_VIOLATION);
    w.unbind();
    drop(w);
    drop(ins);
    drop(upd);
    drop(del);
    drop(src);
    quiesced();
}

