//! M4's host deltas against the in-memory heap (M4-plan §6). H-i (D585): a parked INSERT / UPDATE Node binds its write host once per execution (Decision 6a), so what
//! the host builds from its relation's constraints is this execution's, as ExecInitModifyTable's: the CHECK / NOT NULL
//! states and the stored-generated states are rebuilt at each bind (in the per-bind context, reset at unbind), never kept
//! from an earlier execution, and the node context stays flat.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use common::*;
use datum::Datum;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, Locker, Outcome, PerTuple, Pins, Snapshot, Source, WriteOp, Writer};
use gravity_hosts::ops::modifytable::index_maint::IndexMaint;
use gravity_hosts::ops::modifytable::write_check::CheckSpec;
use gravity_hosts::{take_error, IndexSource, LockHost, SiteRecipe, WriteHost};
use gravity_program::{Cmd, Conflict, IndexPlan, LockKind, WaitPolicy, WritePlan};
use mcx::MemoryContext;
use types_core::Oid;
use types_error::{PgResult, ERRCODE_CHECK_VIOLATION, ERRCODE_NOT_NULL_VIOLATION, ERRCODE_UNIQUE_VIOLATION, ERRCODE_WITH_CHECK_OPTION_VIOLATION};
use types_nodes::parsenodes::WCOKind;
use types_nodes::{Node, NodeList};
use types_rel::{Relation, LOCKMODE};
use types_snapshot::SnapshotData;
use types_storage::lock::{AccessShareLock, RowExclusiveLock};
use types_tuple::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

static FLAG: AtomicU8 = AtomicU8::new(0);

const BINDS: usize = 1000;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn plan(cmd: Cmd) -> WritePlan {
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    WritePlan::write(a, cmd, HEAP_OID, &[IndexPlan::new(a, IDX_OID, &[1], false)])
}

/// D585: the host's constraint states were armed at its first bind and kept by a parked Node; a relation that gained
/// NOT NULL since (a relcache rebuild the host sees at its next bind) let a NULL through: a value where PG errors.
#[test]
fn a_rebound_write_host_reads_the_relations_constraints_of_this_bind() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&plan(Cmd::Insert));
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Insert, ScanPos::NONE, &[0], &[1]).unwrap(), Outcome::Done, "no constraint yet: NULL is written");
    w.unbind();

    set_not_null(true);
    w.bind(&b).unwrap();
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[0], &[1]);
    let e = take_error();
    w.unbind();
    set_not_null(false);
    assert!(r.is_err(), "the rebound host wrote a NULL into a NOT NULL column: {r:?}");
    assert_eq!(e.expect("stashed pg error").sqlstate(), ERRCODE_NOT_NULL_VIOLATION);
    drop(w);
    quiesced();
}

/// D608: the constraint state (the CHECK's ExprStates and its slot) is the bind's, not the row's. WriteHost::write resets
/// its per-row context after every row; H-i built the state there, so from row 2 on the CHECK ran freed ExprStates
/// (debug builds clobber the reset window, as PG's CLOBBER_FREED_MEMORY: deterministic). Five rows through one bind
/// under `CHECK (a IS NOT NULL)`: four are written, the fifth (a NULL) raises PG's 23514.
#[test]
fn a_write_hosts_constraint_state_outlives_its_rows() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    set_check_not_null(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&plan(Cmd::Insert));
    w.bind(&b).unwrap();
    let mut got = Vec::new();
    for v in [5u64, 6, 7, 8] {
        got.push(w.write(WriteOp::Insert, ScanPos::NONE, &[v], &[0]).map_err(|_| take_error().map(|e| e.message)));
    }
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[0], &[1]);
    let e = take_error();
    w.unbind();
    set_check_not_null(false);
    assert_eq!(got, vec![Ok(Outcome::Done); 4], "rows 5..8 pass the CHECK");
    assert!(r.is_err(), "a later row of the bind broke the CHECK and was written: {r:?}");
    assert_eq!(e.expect("stashed pg error").sqlstate(), ERRCODE_CHECK_VIOLATION);
    drop(w);
    quiesced();
}

/// The per-bind rebuild costs the node context nothing: a thousand binds, each writing a row through the armed NOT NULL
/// check, leave it where the first bind left it.
#[test]
fn a_write_host_with_armed_constraints_binds_a_thousand_times_in_flat_memory() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&plan(Cmd::Insert));
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Insert, ScanPos::NONE, &[1], &[0]).unwrap(), Outcome::Done);
    w.unbind();
    let used = w.scratch_used();
    for i in 0..BINDS {
        w.bind(&b).unwrap();
        assert_eq!(w.write(WriteOp::Insert, ScanPos::NONE, &[i as u64 + 2], &[0]).unwrap(), Outcome::Done);
        w.unbind();
    }
    set_not_null(false);
    assert_eq!(w.scratch_used(), used, "{BINDS} binds of a parked write host with armed constraints grew its node context");
    drop(w);
    quiesced();
}

/// D604: a node running as an EPQ recheck never locks again (PG's EvalPlanQual re-evaluates below ExecLockRows' one lock).
/// Its lock answers Done without table_tuple_lock: here on a row this transaction deleted, which a real lock skips
/// (TM_SelfModified) and a lock traversing a concurrent update would turn into Recheck (the recheck's error). Off again
/// after `epq(false)` and after unbind.
#[test]
fn an_epq_recheck_node_does_not_lock_again() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[5, 6]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let st = gravity_program::Arena::new(256);
    let mut del = WriteHost::new(&WritePlan::write(st.handle(), Cmd::Delete, HEAP_OID, &[IndexPlan::new(st.handle(), IDX_OID, &[1], false)]));
    del.bind(&b).unwrap();
    assert_eq!(del.write(WriteOp::Delete, pos(1), &[], &[]).unwrap(), Outcome::Done);
    del.unbind();

    let mut l = LockHost::new(&WritePlan::locking(HEAP_OID, LockKind::KeyShare, WaitPolicy::Block));
    l.bind(&b).unwrap();
    assert_eq!(l.lock(pos(1)).unwrap(), Outcome::Skipped, "a real lock reaches table_tuple_lock");
    l.epq(true);
    assert_eq!(l.lock(pos(1)).unwrap(), Outcome::Done, "the recheck node locked again");
    l.epq(false);
    assert_eq!(l.lock(pos(1)).unwrap(), Outcome::Skipped);
    l.epq(true);
    l.unbind();
    l.bind(&b).unwrap();
    assert_eq!(l.lock(pos(1)).unwrap(), Outcome::Skipped, "unbind ends the recheck");
    l.unbind();
    drop(l);
    drop(del);
    quiesced();
}

type LockFn = fn(Oid, LOCKMODE) -> PgResult<()>;
static LOCK_PREV: OnceLock<(LockFn, LockFn)> = OnceLock::new();
/// The index's RowExclusiveLocks, taken and held now (the tests of this binary run one at a time: `serial`).
static IDX_TAKEN: AtomicU32 = AtomicU32::new(0);
static IDX_HELD: AtomicI32 = AtomicI32::new(0);

fn counting_lock(relid: Oid, mode: LOCKMODE) -> PgResult<()> {
    if relid == IDX_OID && mode == RowExclusiveLock {
        IDX_TAKEN.fetch_add(1, Ordering::Relaxed);
        IDX_HELD.fetch_add(1, Ordering::Relaxed);
    }
    (LOCK_PREV.get().expect("wrapped").0)(relid, mode)
}

fn counting_unlock(relid: Oid, mode: LOCKMODE) -> PgResult<()> {
    if relid == IDX_OID && mode == RowExclusiveLock {
        IDX_HELD.fetch_sub(1, Ordering::Relaxed);
    }
    (LOCK_PREV.get().expect("wrapped").1)(relid, mode)
}

/// (taken since the last call, held now) of the index's RowExclusiveLock.
fn idx_locks() -> (u32, i32) {
    (IDX_TAKEN.swap(0, Ordering::Relaxed), IDX_HELD.load(Ordering::Relaxed))
}

/// D613: a write host locked its target's indexes at the bind and kept the lock to the transaction's end (`IndexMaint::bind`
/// under RowExclusiveLock, the park closing with NoLock), for every command. PostgreSQL opens them at the first row that
/// inserts or updates (ExecInsert, ExecUpdatePrologue: ExecOpenIndices) and releases that lock when the statement ends
/// (ExecCloseIndices); a DELETE never opens them. So an INSERT of no row held every index's lock, and any INSERT kept it
/// to the transaction's end (PostgreSQL: the table's lock alone, m4_d582_index_locks), and a DELETE or an UPDATE run from
/// a generic plan (no planner lock of its own on the indexes) kept locks PostgreSQL never holds past the statement.
#[test]
fn a_write_host_locks_its_indexes_from_the_first_row_to_the_unbind() {
    let _g = serial();
    install();
    LOCK_PREV.get_or_init(|| (lmgr_seams::lock_relation_oid::wrap(counting_lock), lmgr_seams::unlock_relation_oid::wrap(counting_unlock)));
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[5, 6]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    idx_locks();

    let mut ins = WriteHost::new(&plan(Cmd::Insert));
    ins.bind(&b).unwrap();
    assert_eq!(idx_locks(), (0, 0), "an INSERT's bind locks no index (ExecOpenIndices runs at its first row)");
    ins.unbind();
    assert_eq!(idx_locks(), (0, 0), "an INSERT of no row never locked its indexes");
    for round in 0..2 {
        ins.bind(&b).unwrap();
        assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[7 + round], &[0]).unwrap(), Outcome::Done);
        assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[9 + round], &[0]).unwrap(), Outcome::Done);
        assert_eq!(idx_locks(), (1, 1), "the first row opened the index once, under RowExclusiveLock (round {round})");
        ins.unbind();
        assert_eq!(idx_locks(), (0, 0), "the unbind released it, as ExecCloseIndices (round {round})");
    }

    let mut upd = WriteHost::new(&plan(Cmd::Update));
    upd.bind(&b).unwrap();
    assert_eq!(idx_locks(), (0, 0), "an UPDATE's bind locks no index (ExecUpdatePrologue opens them)");
    assert_eq!(upd.write(WriteOp::Update, pos(0), &[15], &[0]).unwrap(), Outcome::Done);
    assert_eq!(idx_locks(), (1, 1));
    upd.unbind();
    assert_eq!(idx_locks(), (0, 0), "the UPDATE's own lock is released at the unbind");

    let mut del = WriteHost::new(&plan(Cmd::Delete));
    del.bind(&b).unwrap();
    assert_eq!(del.write(WriteOp::Delete, pos(1), &[], &[]).unwrap(), Outcome::Done);
    del.unbind();
    assert_eq!(idx_locks(), (0, 0), "a DELETE never opens its target's indexes");
    drop((ins, upd, del));
    quiesced();
}

/// M4-RET (iii): a DELETE whose write site carries cells (DELETE .. RETURNING) hands back the version it deleted, as
/// ExecDelete fetches it under SnapshotAny for RETURNING; one without cells (M1's DELETE end) fetches nothing, and the
/// next row op releases the fetched version (its buffer pin).
#[test]
fn a_returning_delete_hands_back_the_deleted_version() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[41, 42, 43]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let mut del = WriteHost::new(&plan(Cmd::Delete));
    del.bind(&b).unwrap();

    assert_eq!(del.write(WriteOp::Delete, pos(0), &[], &[]).unwrap(), Outcome::Done);
    assert!(!del.written(&mut [], &mut []), "a site without cells hands nothing back");

    assert_eq!(del.write(WriteOp::Delete, pos(1), &[7], &[1]).unwrap(), Outcome::Done);
    let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
    assert!(del.written(&mut v, &mut n), "RETURNING's site reads the deleted row");
    assert_eq!((v[0] as i32, n[0]), (42, 0));

    assert_eq!(del.write(WriteOp::Delete, pos(1), &[7], &[1]).unwrap(), Outcome::Skipped, "this command deleted it already");
    let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
    assert!(!del.written(&mut v, &mut n), "the next row op released the last deleted version");

    assert_eq!(del.write(WriteOp::Delete, pos(2), &[0], &[1]).unwrap(), Outcome::Done);
    del.unbind();
    drop(del);
    quiesced();
}

/// D624 / D765: a write whose plan carries no IndexPlan for one of its relation's indexes (an index created since the
/// plan froze its list, by the statement's own first row or another session; a declared member decompose leaves
/// unplanned) would insert rows that index never saw, where ExecInsertIndexTuples inserts into every ready index. The
/// host reads the list at the first row that writes, and one that is not the plan's takes the executor's own
/// ExecOpenIndices / ExecInsertIndexTuples: the row gets its entry, as in PostgreSQL. A DELETE writes no index entry,
/// and an index not yet ready for inserts (CREATE INDEX CONCURRENTLY's first phase) is one PostgreSQL skips too.
#[test]
fn a_write_whose_plan_misses_a_ready_index_maintains_it_as_the_executor_does() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    set_index_unready(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[5, 6]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let bare = |cmd: Cmd| {
        let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
        WritePlan::write(a, cmd, HEAP_OID, &[])
    };

    for (cmd, op, at, key) in [(Cmd::Insert, WriteOp::Insert, ScanPos::NONE, 7), (Cmd::Update, WriteOp::Update, pos(0), 9)] {
        let mut w = WriteHost::new(&bare(cmd));
        w.bind(&b).unwrap();
        assert_eq!(w.write(op, at, &[key], &[0]).unwrap(), Outcome::Done, "{cmd:?} without the relation's index in its plan writes");
        w.unbind();
        assert!(indexed(ctx.mcx(), &b, key as i32), "{cmd:?}: the index outside the plan has the row's entry (ExecInsertIndexTuples)");
    }
    let mut del = WriteHost::new(&bare(Cmd::Delete));
    del.bind(&b).unwrap();
    assert_eq!(del.write(WriteOp::Delete, pos(1), &[], &[]).unwrap(), Outcome::Done, "a DELETE maintains no index");
    del.unbind();

    set_index_unready(true);
    let mut ins = WriteHost::new(&bare(Cmd::Insert));
    ins.bind(&b).unwrap();
    assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[8], &[0]).unwrap(), Outcome::Done, "an index not ready for inserts gets no entry in PostgreSQL either");
    ins.unbind();
    set_index_unready(false);
    drop((ins, del));
    quiesced();
}

fn unique_plan(cmd: Cmd) -> WritePlan {
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    WritePlan::write(a, cmd, HEAP_OID, &[IndexPlan::new(a, IDX_OID, &[1], true)])
}

/// H-a2: a DEFERRABLE unique index (`!indimmediate`) is inserted with UNIQUE_CHECK_PARTIAL, as ExecInsertIndexTuples
/// does: a duplicate key is no error at the row, and the index is named in the row's recheckIndexes, which the AR
/// triggers hand AfterTriggerSaveEvent for the constraint's deferred unique_key_recheck event. The host passed `&[]`
/// and checked uniqueness at the row (UNIQUE_CHECK_YES), where PostgreSQL defers it to the recheck.
#[test]
fn a_deferrable_unique_index_takes_a_duplicate_and_names_it_for_the_recheck() {
    let _g = serial();
    install();
    set_unique_index(true);
    set_deferrable_index(true);
    set_not_null(false);
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("h-a2")));
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &[10, 20]);
    let pos = ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[0]), ItemPointerGetOffsetNumber(&tids[0]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut ins = WriteHost::new(&unique_plan(Cmd::Insert));
    ins.bind(&b).unwrap();
    assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[20], &[0]).unwrap(), Outcome::Done, "a duplicate on a deferrable index is checked at the recheck, not at the row");
    assert_eq!(ins.write(WriteOp::Insert, ScanPos::NONE, &[30], &[0]).unwrap(), Outcome::Done);
    ins.unbind();
    let mut upd = WriteHost::new(&unique_plan(Cmd::Update));
    upd.bind(&b).unwrap();
    assert_eq!(upd.write(WriteOp::Update, pos, &[30], &[0]).unwrap(), Outcome::Done, "an UPDATE to a duplicate key likewise");
    upd.unbind();

    let heap = Relation::open(heap_relation_data(mcx), None);
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    let mut m = IndexMaint::new(&[IndexPlan::new(a, IDX_OID, &[1], true)]);
    assert!(m.open(mcx, &heap).unwrap());
    for (v, want) in [(10, &[IDX_OID][..]), (40, &[][..]), (40, &[IDX_OID][..]), (50, &[][..])] {
        let mut t = heaptuple::heap_form_tuple(mcx, &heap.rd_att, &[datum::Datum::from_i32(v)], &[false]).unwrap();
        heapam::heap_insert(&heap, t.as_tuple_mut(), 0, 0, None).unwrap();
        let tid = t.as_tuple().t_self;
        m.insert(mcx, &heap, &tid, &[v as u64], &[0], false, false).unwrap();
        assert_eq!(m.rechecks(), want, "the recheckIndexes of the row with key {v}");
    }
    m.unbind().unwrap();
    set_deferrable_index(false);
    drop((ins, upd, heap));
    quiesced();
}

/// H-a2: the admission's names for a deferrable index: a deferrable unique index is maintained (H-a2's UNIQUE_CHECK_PARTIAL
/// and recheck list), admitted with M4-TRIG's T1b; V4R-297: a deferrable exclusion constraint too (the CEOUC_NOWAIT probe).
#[test]
fn a_deferrable_exclusion_constraint_is_maintained() {
    let _g = serial();
    install();
    let ctx = MemoryContext::new("h-a2 names");
    let mcx = ctx.mcx();
    let index = |exclusion: bool| {
        let mut d = index_relation_data(mcx, true);
        let f = d.rd_index.as_mut().unwrap();
        f.indimmediate = false;
        f.indisexclusion = exclusion;
        Relation::open(d, None)
    };
    let name = |r: &Relation<'_>| gravity_hosts::ops::modifytable::index_maint::unmaintainable(r).unwrap_or("maintained");
    assert_eq!(name(&index(true)), "maintained", "a deferrable exclusion constraint (CEOUC_NOWAIT, the recheck list)");
    assert_eq!(name(&index(false)), "maintained", "a deferrable unique index (UNIQUE_CHECK_PARTIAL, the recheck list)");
    assert_eq!(name(&Relation::open(index_relation_data(mcx, true), None)), "maintained", "an immediate unique index");
}

fn indexed(mcx: mcx::Mcx<'_>, b: &BindingRef<'static>, key: i32) -> bool {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, true), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    let mut src = IndexSource::new(SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap());
    src.bind(b).unwrap();
    src.seek(&[key as u64], &[0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    let found = src.next(&mut v, &mut n).unwrap() && v[0] as i32 == key;
    while src.next(&mut v, &mut n).unwrap() {}
    src.unbind();
    found
}

/// A check option `p` on `t` of `kind` whose qual is constant `false` (the implicit-AND list ExecInitQual takes).
fn failing(kind: WCOKind) -> Rc<[CheckSpec]> {
    let qctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("h-b quals")));
    let qm = qctx.mcx();
    let f = Node::mk_const(qm, 16, -1, 0, 1, Datum::from_bool(false), false, true).unwrap();
    let node = Node::mk_list(qm, NodeList::make1(qm, f).unwrap()).unwrap();
    Rc::from(vec![CheckSpec { kind, relname: Arc::from("t"), polname: Some(Arc::from("p")), node, target: 0, inits: Rc::from([]) }])
}

/// An upsert plan; a speculative insert that goes in releases its speculative-insertion lock (lmgr.c
/// SpeculativeInsertionLockRelease), which the fixture's lock manager takes as a no-op.
fn upsert_plan() -> WritePlan {
    if !lock_seams::lock_release::is_installed() {
        lock_seams::lock_release::set(|_tag, _mode, _session| Ok(true));
    }
    let mut p = unique_plan(Cmd::Insert);
    p.conflict = Conflict::Update;
    p
}

/// H-b: ExecInsert's RLS insert check runs on the ON CONFLICT path too, before the row goes in (it let the row in: a
/// value where PG raises 42501).
#[test]
fn an_upsert_insert_runs_the_rls_insert_check() {
    let _g = serial();
    install();
    set_unique_index(true);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&upsert_plan()).with_checks(Some(failing(WCOKind::WCO_RLS_INSERT_CHECK)));
    w.bind(&b).unwrap();
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[30], &[0]);
    let e = take_error();
    w.unbind();
    assert!(r.is_err(), "the upsert's new row passed a failing RLS insert check: {r:?}");
    let e = e.expect("stashed pg error");
    assert_eq!(e.message, "new row violates row-level security policy \"p\" for table \"t\"");
    assert!(!indexed(ctx.mcx(), &b, 30));
    drop(w);
    quiesced();
}

/// H-b (V4R-244): DO UPDATE checks the locked existing row against the UPDATE USING policies (WCO_RLS_CONFLICT_CHECK)
/// when the IR's arm asks (`conflict_check`, after its WHERE and before the SET: ExecOnConflictUpdate's order): the
/// conflict itself hands the row back, the check raises PG's 42501 with the USING text.
#[test]
fn a_do_update_runs_the_rls_conflict_check_on_the_existing_row() {
    let _g = serial();
    install();
    set_unique_index(true);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&upsert_plan()).with_checks(Some(failing(WCOKind::WCO_RLS_CONFLICT_CHECK)));
    w.bind(&b).unwrap();
    let o = w.write(WriteOp::Insert, ScanPos::NONE, &[10], &[0]);
    assert!(matches!(o, Ok(Outcome::Conflict)), "the conflict is the arm's (its WHERE runs before the check): {o:?}");
    let r = w.conflict_check();
    let e = take_error();
    w.unbind();
    assert!(r.is_err(), "the existing row passed a USING policy that rejects it: {r:?}");
    let e = e.expect("stashed pg error");
    assert_eq!(e.message, "new row violates row-level security policy \"p\" (USING expression) for table \"t\"");
    drop(w);
    quiesced();
}

/// H-b (D595): a speculative insert that went in runs the view's check option (ExecInsert's WCO_VIEW_CHECK after
/// the insert; it let the row in: a value where PG raises 44000 with the view's text).
#[test]
fn an_upsert_insert_runs_the_view_check_option() {
    let _g = serial();
    install();
    set_unique_index(true);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&upsert_plan()).with_checks(Some(failing(WCOKind::WCO_VIEW_CHECK)));
    w.bind(&b).unwrap();
    // a NULL key: no arbiter conflict, and the failing row reads "(null)" (no type output call in the fixture)
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[0], &[1]);
    let e = take_error();
    w.unbind();
    assert!(r.is_err(), "the upsert's new row passed a failing view check option: {r:?}");
    let e = e.expect("stashed pg error");
    assert_eq!(e.sqlstate(), ERRCODE_WITH_CHECK_OPTION_VIOLATION);
    assert_eq!(e.message, "new row violates check option for view \"t\"");
    drop(w);
    quiesced();
}

/// H-e (b): a DELETE over several result relations (here an inheritance parent `t` and its child `t2`: leaves[0] is the
/// parent) takes each row to the target of its position's relation, as ExecLookupResultRelByOid picks the row's
/// ResultRelInfo by its tableoid. The two heaps' first rows share a TID, so a row deleted at the wrong target shows. The
/// child's host is built at its first row; a row of a relation outside the targets is a Bug-class error; an UPDATE
/// without the statement's updated columns refuses at the bind (its cells are placed by them).
#[test]
fn a_delete_over_several_result_relations_takes_each_row_to_its_own_target() {
    use gravity_hosts::ops::modifytable::write_route::MultiTargetHost;
    use gravity_program::RouteLeaf;
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let t1 = load_rows(ctx.mcx(), &[1, 2]);
    let t2 = load_heap2_rows(ctx.mcx(), &[3, 4]);
    assert_eq!(t1[0], t2[0], "the fixture's first rows share a TID");
    let at = |rel: Oid, tid: &types_tuple::ItemPointerData| ScanPos::new(rel, ItemPointerGetBlockNumber(tid), ItemPointerGetOffsetNumber(tid));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let multi = |cmd: Cmd| {
        let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
        let leaves = [RouteLeaf { relid: HEAP_OID, natts: 1, nindexes: 0, before_row: false }, RouteLeaf { relid: HEAP2_OID, natts: 1, nindexes: 0, before_row: false }];
        WritePlan::write(a, cmd, HEAP_OID, &[]).with_route(a, HEAP_OID, &leaves)
    };

    let mut w = MultiTargetHost::new(&multi(Cmd::Delete));
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Delete, at(HEAP2_OID, &t2[0]), &[], &[]).unwrap(), Outcome::Done, "the child's row, at the child");
    assert_eq!(w.write(WriteOp::Delete, at(HEAP_OID, &t1[0]), &[], &[]).unwrap(), Outcome::Done, "the parent's row at the same TID is still there");
    assert_eq!(w.write(WriteOp::Delete, at(HEAP2_OID, &t2[0]), &[], &[]).unwrap(), Outcome::Skipped, "this command deleted the child's row");
    assert_eq!(w.write(WriteOp::Delete, at(HEAP2_OID, &t2[1]), &[], &[]).unwrap(), Outcome::Done);
    assert!(w.write(WriteOp::Delete, at(HEAP_OID + 99, &t1[1]), &[], &[]).is_err(), "a row outside the targets");
    let e = take_error().expect("stashed pg error");
    assert!(e.message.starts_with("gravity: Reason::Bug: a Delete of a row of relation 61099 outside"), "{}", e.message);
    assert_eq!(w.write(WriteOp::Delete, at(HEAP_OID, &t1[1]), &[], &[]).unwrap(), Outcome::Done, "the host goes on after the refused row");
    w.unbind();

    let mut u = MultiTargetHost::new(&multi(Cmd::Update));
    assert_eq!(u.bind(&b), Err(gravity_hosts::unsupported()), "an UPDATE without its updated columns");
    u.unbind();
    drop((w, u));
    quiesced();
}

/// M4-UPDATE: an UPDATE over several result relations. Its site carries the SET cells first (R5, D764: under RETURNING as
/// wide as the widest target, whose new row `written` reads back); the host fetches the old
/// version at the row's position from the target the position names (SnapshotAny), places each cell at the target
/// attribute its root column names, and updates that target through its own write host (its indexes maintained: `t`'s
/// one index, re-read at its first row). The two heaps' first rows share a TID, so a row written at the wrong target
/// shows; each new version lands at the page's next offset (the fixture's heap), where the test reads it.
#[test]
fn an_update_over_several_result_relations_forms_each_targets_row_over_its_old_version() {
    use gravity_hosts::ops::modifytable::write_route::MultiTargetHost;
    use gravity_hosts::ops::modifytable::write_constraints::ModifiedCols;
    use gravity_program::RouteLeaf;
    use types_nodes::Bitmapset;
    use types_tuple::htup::FirstLowInvalidHeapAttributeNumber as FLIHAN;
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let t1 = load_rows(ctx.mcx(), &[1, 2]);
    let t2 = load_heap2_rows(ctx.mcx(), &[3, 4]);
    assert_eq!(t1[0], t2[0], "the fixture's first rows share a TID");
    let at = |rel: Oid, tid: &types_tuple::ItemPointerData| ScanPos::new(rel, ItemPointerGetBlockNumber(tid), ItemPointerGetOffsetNumber(tid));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let stmt = MemoryContext::new("the statement's plan");
    let sm = stmt.mcx();
    let ins = mcx::alloc_leak_in(sm, Bitmapset::empty()).unwrap();
    let upd = mcx::alloc_leak_in(sm, Bitmapset::make_singleton(sm, 1 - FLIHAN).unwrap()).unwrap();
    // SAFETY: the plan's bitmaps as the admission hands them in (`admit::modified_cols`: 'static over the statement's plan)
    let cols: ModifiedCols = unsafe { (&*(ins as *const Bitmapset<'_> as *const Bitmapset<'static>), &*(upd as *const Bitmapset<'_> as *const Bitmapset<'static>)) };
    let a = Box::leak(Box::new(gravity_program::Arena::new(256))).handle();
    let leaves = [RouteLeaf { relid: HEAP_OID, natts: 1, nindexes: 1, before_row: false }, RouteLeaf { relid: HEAP2_OID, natts: 1, nindexes: 0, before_row: false }];
    let plan = WritePlan::write(a, Cmd::Update, HEAP_OID, &[]).with_route(a, HEAP_OID, &leaves);

    let mut u = MultiTargetHost::new(&plan).with_modified(Some(cols));
    u.bind(&b).unwrap();
    assert_eq!(u.write(WriteOp::Update, at(HEAP2_OID, &t2[0]), &[300], &[0]).unwrap(), Outcome::Done, "the child's row, at the child");
    // R5 (D764): RETURNING reads the target's new row back, in the target's layout
    let (mut v, mut n) = ([0; 1], [1; 1]);
    assert!(u.written(&mut v, &mut n), "an UPDATE's new row is read back");
    assert_eq!((v, n), ([300], [0]), "the child's new row");
    assert_eq!(u.write(WriteOp::Update, at(HEAP_OID, &t1[0]), &[100], &[0]).unwrap(), Outcome::Done, "the parent's row at the same TID");
    assert_eq!(u.write(WriteOp::Update, at(HEAP2_OID, &t2[0]), &[301], &[0]).unwrap(), Outcome::Skipped, "this command updated the child's row");
    // a site wider than the SET cells is R5's (RETURNING); fewer cells than the SET columns is a bug
    assert!(u.write(WriteOp::Update, at(HEAP_OID, &t1[1]), &[], &[]).is_err(), "no SET cell where the statement updates one column");
    let e = take_error().expect("stashed pg error");
    assert!(e.message.starts_with("gravity: Reason::Bug: an UPDATE row of 0 cells"), "{}", e.message);
    assert!(!u.written(&mut [0], &mut [1]), "a failed row is not read back");
    u.unbind();
    drop(stmt); // the deriving statement ends: a parked host reads its own copies of the columns from here on

    let m = ctx.mcx();
    for (rel, data, want) in [(HEAP_OID, heap_relation_data(m), 100), (HEAP2_OID, heap2_relation_data(m), 300)] {
        let heap = Relation::open(data, None);
        let mut slot = tableam::table_slot_create(m, &heap).unwrap();
        let new = types_tuple::ItemPointerData::new(ItemPointerGetBlockNumber(&t1[0]), 3);
        assert!(gravity_hosts::ops::shared::heap_fetch::fetch_any(m, &heap, &new, &mut slot).unwrap(), "{rel}: the new version");
        let mut null = false;
        assert_eq!((exectuples::slot_getattr(&mut slot, 1, &mut null).as_i32(), null), (want, false), "{rel}");
        exectuples::exec_clear_tuple(&mut slot, m);
    }
    u.bind(&b).unwrap();
    assert_eq!(u.write(WriteOp::Update, at(HEAP_OID, &t1[1]), &[200], &[0]).unwrap(), Outcome::Done, "a rebind after the statement's plan is gone");
    u.unbind();
    drop(u);
    quiesced();
}

type SatUpdateFn = for<'a, 'tup> fn(&'a mut types_tuple::HeapTupleData<'tup>, types_core::CommandId, types_core::Buffer) -> PgResult<tableam_vocab::TM_Result>;
static SAT_PREV: OnceLock<SatUpdateFn> = OnceLock::new();
/// heap_tuple_satisfies_update calls: one per heap_lock_tuple (the lock's own test of a version).
static SAT_CALLS: AtomicU32 = AtomicU32::new(0);

fn counting_satisfies_update(h: &mut types_tuple::HeapTupleData<'_>, cid: types_core::CommandId, buf: types_core::Buffer) -> PgResult<tableam_vocab::TM_Result> {
    SAT_CALLS.fetch_add(1, Ordering::Relaxed);
    (SAT_PREV.get().expect("wrapped"))(h, cid, buf)
}

/// A version's header on its page, edited in place (the fixture's pages are the buffers).
fn edit_header(tid: &types_tuple::ItemPointerData, f: impl FnOnce(&mut types_tuple::HeapTupleHeaderData)) {
    edit_header_in(heap_relation_data, tid, f)
}

/// `edit_header` on the heap `rd` describes (H-h: the second heap `t2`).
fn edit_header_in(rd: fn(mcx::Mcx<'_>) -> types_rel::RelationData<'_>, tid: &types_tuple::ItemPointerData, f: impl FnOnce(&mut types_tuple::HeapTupleHeaderData)) {
    let ctx = MemoryContext::new("hdr");
    let heap = types_rel::Relation::open(rd(ctx.mcx()), None);
    let buf = bufmgr_seams::read_buffer::call(&heap, ItemPointerGetBlockNumber(tid)).unwrap();
    let page = bufmgr_seams::buffer_get_page::call(buf).as_ptr();
    let off = ItemPointerGetOffsetNumber(tid) as usize;
    // SAFETY: the fixture's page, pinned; the line pointer array follows the 24-byte page header, lp_off its low 15 bits
    unsafe {
        let lp = core::ptr::read_unaligned(page.add(24 + 4 * (off - 1)) as *const u32);
        f(&mut *(page.add((lp & 0x7fff) as usize) as *mut types_tuple::HeapTupleHeaderData));
    }
    bufmgr_seams::release_buffer::call(buf).unwrap();
}

/// RL1b: a lock that follows the update chain (read committed, TUPLE_LOCK_FLAG_FIND_LAST_VERSION) locks the newest
/// version and answers Recheck; `existing` then hands back that version's cells and position (ExecLockRows' EPQ test
/// tuple) from the slot the lock filled, without locking again (D604: one heap_lock_tuple per version, two for the
/// traversal, none for `existing`). A lock that does not traverse leaves nothing to hand back.
#[test]
fn a_traversed_lock_hands_back_the_newest_version_without_locking_again() {
    let _g = serial();
    install();
    SAT_PREV.get_or_init(|| heapam_visibility_seams::heap_tuple_satisfies_update::wrap(counting_satisfies_update));
    if !heapam_visibility_seams::heap_tuple_satisfies_dirty::is_installed() {
        heapam_visibility_seams::heap_tuple_satisfies_dirty::set(|_h, _snap, _buf| Ok(true));
    }
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[7, 70, 8]);
    // v1 (7) updated by committed xact 20 to v2 (70): v1's xmax 20, its ctid v2; v2's xmin 20
    let (v1, v2) = (tids[0], tids[1]);
    edit_header(&v1, |h| {
        h.set_xmax(20);
        h.t_infomask &= !(types_tuple::HEAP_XMAX_INVALID | types_tuple::HEAP_XMAX_LOCK_ONLY);
        h.t_ctid = v2;
    });
    edit_header(&v2, |h| h.set_xmin(20));
    let pos = |t: &types_tuple::ItemPointerData| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(t), ItemPointerGetOffsetNumber(t));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut l = LockHost::new(&WritePlan::locking(HEAP_OID, LockKind::Exclusive, WaitPolicy::Block));
    l.bind(&b).unwrap();
    let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
    assert!(l.existing(&mut v, &mut n).is_err(), "nothing traversed yet");
    SAT_CALLS.store(0, Ordering::Relaxed);
    assert_eq!(l.lock(pos(&v1)).unwrap(), Outcome::Recheck, "the lock followed v1's update to v2");
    assert_eq!(SAT_CALLS.load(Ordering::Relaxed), 2, "v1 tested, then v2 locked");
    assert_eq!(l.recheck_pos(), pos(&v2));
    assert_eq!(l.existing(&mut v, &mut n).unwrap(), pos(&v2));
    assert_eq!((v[0] as i32, n[0]), (70, 0), "v2's cells");
    assert_eq!(SAT_CALLS.load(Ordering::Relaxed), 2, "existing locked nothing");

    assert_eq!(l.lock(pos(&tids[2])).unwrap(), Outcome::Done);
    assert!(l.existing(&mut v, &mut n).is_err(), "a lock that did not traverse leaves nothing to hand back");
    l.unbind();
    drop(l);
    quiesced();
}

/// The stored generated column's row is the bind's to compute, but not to keep: PG's ExecComputeStoredGenerated
/// materializes into the result relation's slot, whose image the next row's clear pfrees. The host formed a new heap
/// image per row in its per-bind context (a bump context, reset only at unbind), so a long INSERT grew it by a row
/// image per row. Here a thousand rows through one bind of `a GENERATED ALWAYS AS (7) STORED` (each written as 7)
/// leave the per-bind context where the second row left it.
#[test]
fn a_generated_column_computes_a_thousand_rows_in_flat_bind_memory() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    set_generated(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[10]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut w = WriteHost::new(&plan(Cmd::Insert));
    w.bind(&b).unwrap();
    let mut got = Vec::new();
    for v in [1u64, 2] {
        got.push(w.write(WriteOp::Insert, ScanPos::NONE, &[v], &[0]).map_err(|_| take_error().map(|e| e.message)));
    }
    let (mut c, mut n) = ([0u64; 1], [1u8; 1]);
    let written = w.written(&mut c, &mut n);
    let used = w.bind_scratch_used();
    for v in 0..BINDS as u64 {
        got.push(w.write(WriteOp::Insert, ScanPos::NONE, &[v], &[0]).map_err(|_| take_error().map(|e| e.message)));
    }
    let after = w.bind_scratch_used();
    w.unbind();
    set_generated(false);
    assert!(got.iter().all(|r| *r == Ok(Outcome::Done)), "every row written: {:?}", got.iter().find(|r| **r != Ok(Outcome::Done)));
    assert!(written && (c[0] as i32, n[0]) == (7, 0), "the written row is the computed one: {written} {c:?} {n:?}");
    assert_eq!(after, used, "{BINDS} rows of one bind grew its per-bind context");
    drop(w);
    quiesced();
}

/// The BEFORE ROW INSERT trigger function of `excluded_is_the_row_execinsert_formed`: NEW's column + 100 (a returned
/// tuple other than NEW, as a PL function that assigns NEW.a returns).
const ADD_100_FOID: Oid = 99_001;

fn add_100(_fl: Option<&mut types_fmgr::FmgrInfo>, fcinfo: &mut types_fmgr::FunctionCallInfoBaseData) -> PgResult<Datum> {
    // SAFETY: the trigger call keeps its TriggerData live for the call
    let td = unsafe { types_trigger_call::trigger_data_from_fcinfo(fcinfo) }.expect("called as a trigger");
    // SAFETY: a BEFORE ROW INSERT call's NEW, live for the call
    let new = unsafe { td.tg_trigtuple.expect("NEW").as_ref() };
    let copy = heaptuple::heap_copytuple(fcinfo.result_mcx(), new)?;
    let (hdr, len, tself, oid, hoff) = (copy.header_ptr(), copy.t_len, copy.t_self, copy.t_tableOid, copy.t_data().t_hoff as usize);
    // SAFETY: the copy's image: its one int4 column, not null, at t_hoff
    unsafe {
        let a = (hdr as *mut u8).add(hoff) as *mut i32;
        a.write_unaligned(a.read_unaligned() + 100);
    }
    core::mem::forget(copy);
    // SAFETY: the forgotten copy's image, live in the call's result context
    let view = unsafe { types_tuple::HeapTupleData::from_raw_parts(hdr, len, tself, oid) };
    Ok(Datum::from_usize(Box::leak(Box::new(view)) as *mut types_tuple::HeapTupleData<'_> as usize))
}

static FMGR_PREV: OnceLock<fn(Oid) -> PgResult<types_fmgr::FmgrInfo>> = OnceLock::new();

fn fmgr_with_add_100(foid: Oid) -> PgResult<types_fmgr::FmgrInfo> {
    if foid == ADD_100_FOID {
        return Ok(types_fmgr::FmgrInfo::new(add_100, foid, 0, false, false));
    }
    (FMGR_PREV.get().expect("wrapped"))(foid)
}

/// The heap's trigger descriptor: one BEFORE ROW INSERT trigger calling `add_100`.
fn add_100_desc() -> Rc<types_trigger::TriggerDesc<'static>> {
    use mcx::{PgString, PgVec};
    use types_trigger::{Trigger, TriggerDesc, TRIGGER_TYPE_BEFORE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_ROW};
    let m = Box::leak(Box::new(MemoryContext::new("trigdesc"))).mcx();
    let t = Trigger { tgoid: 99_002, tgname: PgString::from_str_in("t_add_100", m).unwrap(), tgfoid: ADD_100_FOID, tgtype: TRIGGER_TYPE_ROW | TRIGGER_TYPE_BEFORE | TRIGGER_TYPE_INSERT, tgenabled: b'O' as i8, tgisinternal: false, tgisclone: false, tgconstrrelid: 0, tgconstrindid: 0, tgconstraint: 0, tgdeferrable: false, tginitdeferred: false, tgnargs: 0, tgnattr: 0, tgattr: PgVec::new_in(m), tgargs: PgVec::new_in(m), tgqual: None, tgoldtable: None, tgnewtable: None };
    let mut v = PgVec::new_in(m);
    v.push(t);
    Rc::new(TriggerDesc { triggers: v, trig_insert_before_row: true, trig_insert_after_row: false, trig_insert_instead_row: false, trig_insert_before_statement: false, trig_insert_after_statement: false, trig_update_before_row: false, trig_update_after_row: false, trig_update_instead_row: false, trig_update_before_statement: false, trig_update_after_statement: false, trig_delete_before_row: false, trig_delete_after_row: false, trig_delete_instead_row: false, trig_delete_before_statement: false, trig_delete_after_statement: false, trig_truncate_before_statement: false, trig_truncate_after_statement: false, trig_insert_new_table: false, trig_update_old_table: false, trig_update_new_table: false, trig_delete_old_table: false })
}

/// V4R-243 (D661 host (a)): ON CONFLICT DO UPDATE's EXCLUDED is the row ExecInsert formed before its arbiter check:
/// NEW as the BEFORE ROW INSERT triggers returned it, then the STORED generated columns computed over it. The write
/// host hands exactly that row back after a Conflict (`written`), which the driver's write_in now puts in the write
/// site. The conflicting key is the formed row's, never the proposed one's:
/// - a trigger making the proposed 10 into 110 conflicts with the existing 110, and EXCLUDED.a is 110; a proposed 20
///   (120) goes in;
/// - under `a GENERATED ALWAYS AS (7) STORED` any proposed value conflicts with the existing 7, and EXCLUDED.a is 7.
#[test]
fn excluded_is_the_row_execinsert_formed() {
    let _g = serial();
    install();
    static SEAMS: std::sync::Once = std::sync::Once::new();
    SEAMS.call_once(|| {
        // TriggerEnabled reads session_replication_role (the default: origin)
        guc_tables::vars::SessionReplicationRole.install_if_absent(guc_tables::GucVarAccessors { get: || guc_tables::consts::SESSION_REPLICATION_ROLE_ORIGIN, set: |_| {} });
        relcache_seams::relation_get_trigger_desc::set(|relid| Ok(if relid == HEAP_OID { TRIG_DESC.with(|d| d.borrow().clone()) } else { None }));
        if fmgr_seams::fmgr_info::is_installed() {
            FMGR_PREV.get_or_init(|| fmgr_seams::fmgr_info::wrap(fmgr_with_add_100));
        } else {
            FMGR_PREV.get_or_init(|| |foid| Err(Box::new(types_error::PgError::error(format!("no function {foid} in the fixture")))));
            fmgr_seams::fmgr_info::set(fmgr_with_add_100);
        }
    });
    set_unique_index(true);
    set_not_null(false);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let written = |w: &mut WriteHost| {
        let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
        w.written(&mut v, &mut n).then_some((v[0] as i32, n[0]))
    };

    // a BEFORE ROW INSERT trigger's NEW
    TRIG_DESC.with(|d| *d.borrow_mut() = Some(add_100_desc()));
    set_has_triggers(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[110]);
    let mut w = WriteHost::new(&upsert_plan());
    w.bind(&b).unwrap();
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[10], &[0]).map_err(|_| take_error().map(|e| e.message));
    assert_eq!(r, Ok(Outcome::Conflict), "NEW 110 (the trigger's) conflicts with the existing 110");
    assert_eq!(written(&mut w), Some((110, 0)), "EXCLUDED is the trigger's row");
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[20], &[0]).map_err(|_| take_error().map(|e| e.message));
    assert_eq!(r, Ok(Outcome::Done), "NEW 120 goes in");
    assert_eq!(written(&mut w), Some((120, 0)));
    w.unbind();
    drop(w);
    set_has_triggers(false);
    TRIG_DESC.with(|d| *d.borrow_mut() = None);
    quiesced();

    // the STORED generated column
    set_generated(true);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    load_rows(ctx.mcx(), &[7]);
    let mut w = WriteHost::new(&upsert_plan());
    w.bind(&b).unwrap();
    let r = w.write(WriteOp::Insert, ScanPos::NONE, &[5], &[0]).map_err(|_| take_error().map(|e| e.message));
    assert_eq!(r, Ok(Outcome::Conflict), "the computed 7 conflicts with the existing 7");
    assert_eq!(written(&mut w), Some((7, 0)), "EXCLUDED's generated column is the computed value");
    w.unbind();
    drop(w);
    set_generated(false);
    quiesced();
}

/// H-h (1): a lock whose position names another relation than the recipe's heap (a leaf of a partitioned or inherited
/// mark: ExecLockRows locks each child's row through its own ExecRowMark) locks the row in THAT relation, and the
/// traversed version it hands back carries that relation. The two heaps' rows share TIDs, so a lock taken on the
/// recipe's heap instead shows: the parent locked t's row at v1's TID (no update chain there: Done), where t2's v1 was
/// updated to v2 (Recheck, v2's cells and position in t2).
#[test]
fn a_lock_takes_the_row_in_the_relation_its_position_names() {
    let _g = serial();
    install();
    SAT_PREV.get_or_init(|| heapam_visibility_seams::heap_tuple_satisfies_update::wrap(counting_satisfies_update));
    if !heapam_visibility_seams::heap_tuple_satisfies_dirty::is_installed() {
        heapam_visibility_seams::heap_tuple_satisfies_dirty::set(|_h, _snap, _buf| Ok(true));
    }
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let t1 = load_rows(ctx.mcx(), &[1, 2, 3]);
    let t2 = load_heap2_rows(ctx.mcx(), &[7, 70, 8]);
    assert_eq!((t1[0], t1[2]), (t2[0], t2[2]), "the fixture's heaps share TIDs");
    let (v1, v2) = (t2[0], t2[1]);
    edit_header_in(heap2_relation_data, &v1, |h| {
        h.set_xmax(20);
        h.t_infomask &= !(types_tuple::HEAP_XMAX_INVALID | types_tuple::HEAP_XMAX_LOCK_ONLY);
        h.t_ctid = v2;
    });
    edit_header_in(heap2_relation_data, &v2, |h| h.set_xmin(20));
    let pos = |rel: Oid, t: &types_tuple::ItemPointerData| ScanPos::new(rel, ItemPointerGetBlockNumber(t), ItemPointerGetOffsetNumber(t));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut l = LockHost::new(&WritePlan::locking(HEAP_OID, LockKind::Exclusive, WaitPolicy::Block));
    for _ in 0..2 {
        // twice: the leaf is opened at its first row of each bind and parked at unbind (the second bind's locks meet
        // this transaction's own: the same answers)
        l.bind(&b).unwrap();
        assert_eq!(l.lock(pos(HEAP2_OID, &v1)).unwrap(), Outcome::Recheck, "t2's v1 was updated to v2");
        assert_eq!(l.recheck_pos(), pos(HEAP2_OID, &v2));
        let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
        assert_eq!(l.existing(&mut v, &mut n).unwrap(), pos(HEAP2_OID, &v2));
        assert_eq!((v[0] as i32, n[0]), (70, 0), "t2's v2 cells");
        assert_eq!(l.lock(pos(HEAP_OID, &t1[2])).unwrap(), Outcome::Done, "the recipe's heap's own row");
        assert_eq!(l.lock(pos(HEAP2_OID, &t2[2])).unwrap(), Outcome::Done);
        l.unbind();
    }
    drop(l);
    quiesced();
}

/// H-f: ExecMergeMatched checks the target's current version against the UPDATE / DELETE USING policies
/// (WCO_RLS_MERGE_UPDATE_CHECK / WCO_RLS_MERGE_DELETE_CHECK) before the action, with the "target row" text (the host
/// updated / deleted it: a value where PG raises 42501). The check reads the old row, whatever the new cells are; a
/// MERGE-kind option is never run by a plain UPDATE (G4), and an action's check never runs for the other action.
#[test]
fn a_merge_action_runs_the_rls_using_check_on_the_target_row() {
    let _g = serial();
    install();
    set_unique_index(false);
    set_not_null(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let tids = load_rows(ctx.mcx(), &[10, 20, 30]);
    let pos = |i: usize| ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[i]), ItemPointerGetOffsetNumber(&tids[i]));
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    for (op, kind, at) in [(WriteOp::Update, WCOKind::WCO_RLS_MERGE_UPDATE_CHECK, 0), (WriteOp::Delete, WCOKind::WCO_RLS_MERGE_DELETE_CHECK, 1)] {
        let mut w = WriteHost::new(&plan(Cmd::Merge)).with_checks(Some(failing(kind)));
        w.bind(&b).unwrap();
        let r = if op == WriteOp::Update { w.write(op, pos(at), &[11], &[0]) } else { w.write(op, pos(at), &[], &[]) };
        let e = take_error();
        w.unbind();
        assert!(r.is_err(), "MERGE {op:?} wrote a row its USING policy rejects: {r:?}");
        let e = e.expect("stashed pg error");
        assert_eq!(e.message, "target row violates row-level security policy \"p\" (USING expression) for table \"t\"");
        drop(w);
    }

    // the other action's option and a plain UPDATE leave the row op alone
    let mut w = WriteHost::new(&plan(Cmd::Merge)).with_checks(Some(failing(WCOKind::WCO_RLS_MERGE_DELETE_CHECK)));
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Update, pos(0), &[11], &[0]).unwrap(), Outcome::Done);
    w.unbind();
    drop(w);
    let mut w = WriteHost::new(&plan(Cmd::Update)).with_checks(Some(failing(WCOKind::WCO_RLS_MERGE_UPDATE_CHECK)));
    w.bind(&b).unwrap();
    assert_eq!(w.write(WriteOp::Update, pos(2), &[31], &[0]).unwrap(), Outcome::Done);
    w.unbind();
    drop(w);
    quiesced();
}
