use std::rc::Rc;

use ::datum::Datum;
use ::mcx::{Mcx, MemoryContext, PgVec};
use ::types_slot::{SlotData, TupleSlotKind};
use ::types_tuple::{
    CompactAttribute, FormData_pg_attribute, TupleDescData, TYPALIGN_INT, TYPSTORAGE_PLAIN,
};

use crate::*;

fn leaked_mcx() -> Mcx<'static> {
    let m: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("tuplestore-test")));
    m.mcx()
}

fn int4_desc(mcx: Mcx<'static>, natts: i32) -> Rc<TupleDescData<'static>> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    for i in 0..natts {
        let att = FormData_pg_attribute {
            attnum: (i + 1) as i16,
            atttypid: 23,
            attlen: 4,
            attbyval: true,
            attalign: TYPALIGN_INT,
            attstorage: TYPSTORAGE_PLAIN,
            ..Default::default()
        };
        compact.push(CompactAttribute::populate_from(&att));
        attrs.push(att);
    }
    Rc::new(TupleDescData {
        natts,
        tdtypeid: 2249,
        tdtypmod: -1,
        tdrefcount: -1,
        constr: None,
        compact_attrs: compact,
        attrs,
    })
}

fn read_i32(slot: &mut SlotData<'_>) -> i32 {
    exectuples::slot_getallattrs(slot);
    assert!(!slot.base().tts_isnull[0]);
    slot.base().tts_values[0].as_i32()
}

fn put_i32(ts: &mut Tuplestore, desc: &TupleDescData<'_>, v: i32) {
    ts.putvalues(desc, &[Datum::from_i32(v)], &[false]).unwrap();
}

#[test]
fn putvalues_gettupleslot_roundtrip() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    for v in 0..100 {
        put_i32(&mut ts, &desc, v);
    }
    assert_eq!(ts.tuple_count(), 100);
    assert!(!ts.ateof());

    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    for v in 0..100 {
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), v);
    }
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(slot.base().is_empty());
    assert!(ts.ateof());
    ts.end();
}

#[test]
fn eof_reader_advances_with_writes() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    put_i32(&mut ts, &desc, 1);
    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    put_i32(&mut ts, &desc, 2);
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
}

#[test]
fn puttupleslot_copies_out_of_virtual_slot() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 2);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    let mut vslot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::Virtual, Some(desc.clone()));
    {
        let base = vslot.base_mut();
        base.tts_values[0] = Datum::from_i32(7);
        base.tts_values[1] = Datum::from_i32(9);
        base.tts_isnull[0] = false;
        base.tts_isnull[1] = false;
    }
    exectuples::exec_store_virtual_tuple(&mut vslot);
    ts.puttupleslot(&mut vslot, mcx).unwrap();
    exectuples::exec_clear_tuple(&mut vslot, mcx);

    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    exectuples::slot_getallattrs(&mut slot);
    assert_eq!(slot.base().tts_values[0].as_i32(), 7);
    assert_eq!(slot.base().tts_values[1].as_i32(), 9);
}

#[test]
fn copy_true_survives_clear() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    put_i32(&mut ts, &desc, 42);
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    assert!(ts.gettupleslot(true, true, &mut slot, mcx).unwrap());
    ts.clear();
    assert_eq!(read_i32(&mut slot), 42);
    assert_eq!(ts.tuple_count(), 0);
    exectuples::exec_clear_tuple(&mut slot, mcx);
}

#[test]
fn clear_then_reuse_and_rescan() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    for v in 0..10 {
        put_i32(&mut ts, &desc, v);
    }
    ts.clear();
    put_i32(&mut ts, &desc, 99);
    assert_eq!(ts.tuple_count(), 1);
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 99);
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    ts.rescan().unwrap();
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 99);
}

#[test]
fn grow_memtuples_past_initial_size() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 4096);
    let n = (INITIAL_MEMTUPSIZE * 3) as i32;
    for v in 0..n {
        put_i32(&mut ts, &desc, v);
    }
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    for v in 0..n {
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), v);
    }
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
}

#[test]
fn backward_walks_to_start_then_none() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    for v in [1, 2, 3] {
        put_i32(&mut ts, &desc, v);
    }
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    for v in [1, 2, 3] {
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), v);
    }
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(ts.ateof());

    for v in [3, 2, 1] {
        assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), v);
    }
    assert!(!ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert!(!ts.ateof());

    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 1);
}

#[test]
fn backward_before_eof_returns_tuple_before_last() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    for v in [1, 2, 3] {
        put_i32(&mut ts, &desc, v);
    }
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 2);
    assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 1);
}

#[test]
fn backward_at_start_is_none_and_rescan_resets() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    put_i32(&mut ts, &desc, 7);
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    assert!(!ts.gettupleslot(false, false, &mut slot, mcx).unwrap());

    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    ts.rescan().unwrap();
    assert!(!ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 7);
}

#[test]
fn follower_read_pointer_replays_leader_fill() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    ts.set_eflags(EXEC_FLAG_REWIND).unwrap();
    let follower = ts.alloc_read_pointer(EXEC_FLAG_REWIND).unwrap();
    assert_eq!(follower, 1);

    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
    put_i32(&mut ts, &desc, 10);
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 10);
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    put_i32(&mut ts, &desc, 20);
    put_i32(&mut ts, &desc, 30);
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());

    ts.select_read_pointer(follower).unwrap();
    for v in [10, 20, 30] {
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), v);
    }
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(ts.ateof());

    ts.rescan().unwrap();
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 10);

    ts.select_read_pointer(0).unwrap();
    assert!(ts.ateof());
}

#[test]
fn inactive_eof_pointer_advances_to_next_write() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    let follower = ts.alloc_read_pointer(EXEC_FLAG_REWIND).unwrap();

    put_i32(&mut ts, &desc, 1);
    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
    ts.select_read_pointer(follower).unwrap();
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());

    ts.select_read_pointer(0).unwrap();
    put_i32(&mut ts, &desc, 2);
    ts.select_read_pointer(follower).unwrap();
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 2);
}

#[test]
fn new_pointer_copies_pointer_zero_position() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, true, 64);
    for v in [1, 2, 3] {
        put_i32(&mut ts, &desc, v);
    }
    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    let p = ts.alloc_read_pointer(EXEC_FLAG_REWIND).unwrap();
    ts.select_read_pointer(p).unwrap();
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 2);
}

#[test]
#[should_panic(expected = "too late to require new tuplestore eflags")]
fn late_eflags_increase_is_loud() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    put_i32(&mut ts, &desc, 1);
    ts.alloc_read_pointer(EXEC_FLAG_BACKWARD).unwrap();
}

#[test]
fn hold_registry_roundtrip_and_staleness() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let h = hold::register(Tuplestore::begin_heap(false, true, 64));
    hold::with_store(h, |ts| put_i32(ts, &desc, 5));

    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    hold::with_store(h, |ts| {
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    });
    assert_eq!(read_i32(&mut slot), 5);
    exectuples::exec_clear_tuple(&mut slot, mcx);

    hold::end(h);
    hold::end(h);
    let stale = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hold::with_store(h, |ts| ts.tuple_count())
    }));
    assert!(stale.is_err());
    hold::end(types_portal::TuplestoreHandle::NULL);
}
#[test]
fn putvalues_packs_varlena_short_form() {
    let mcx = leaked_mcx();
    let att = FormData_pg_attribute {
        attnum: 1,
        atttypid: 25,
        attlen: -1,
        attbyval: false,
        attalign: TYPALIGN_INT,
        attstorage: b'x' as i8,
        ..Default::default()
    };
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    compact.push(CompactAttribute::populate_from(&att));
    attrs.push(att);
    let desc = Rc::new(TupleDescData {
        natts: 1, tdtypeid: 2249, tdtypmod: -1, tdrefcount: -1,
        constr: None, compact_attrs: compact, attrs,
    });

    let mut image: Vec<u8> = vec![];
    let payload = b"4MB";
    let hdr = ((payload.len() + 4) as u32) << 2;
    image.extend_from_slice(&hdr.to_ne_bytes());
    image.extend_from_slice(payload);
    let image = Box::leak(image.into_boxed_slice());
    let d = Datum::from_usize(image.as_ptr() as usize);

    let mtup = heaptuple::heap_form_minimal_tuple(mcx, &desc, &[d], &[false], 0).unwrap();
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));
    exectuples::exec_store_minimal_tuple_owned(&mut slot, mcx, mtup);
    exectuples::slot_getallattrs(&mut slot);
    let out = slot.base().tts_values[0];
    let p = out.as_usize() as *const u8;
    let b0 = unsafe { *p };
    assert_eq!(b0 & 0x01, 1, "short-form varlena header");
    assert_eq!((b0 >> 1) as usize, 1 + payload.len());
    let data = unsafe { std::slice::from_raw_parts(p.add(1), payload.len()) };
    assert_eq!(data, payload);
}

#[test]
fn skiptuples_and_advance_window_navigation() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, true, 64);
    ts.set_eflags(0).unwrap();
    let rp = ts.alloc_read_pointer(::types_slot::EXEC_FLAG_BACKWARD).unwrap();
    for v in 0..10 {
        put_i32(&mut ts, &desc, v);
    }
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc));

    ts.select_read_pointer(rp).unwrap();
    assert!(ts.skiptuples(4, true).unwrap());
    assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 4);

    assert!(ts.advance(true).unwrap());
    assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 4);

    assert!(ts.skiptuples(3, false).unwrap());
    assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 0);

    assert!(!ts.skiptuples(50, true).unwrap());
    assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
    assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
    assert_eq!(read_i32(&mut slot), 9);
    ts.end();
}

#[test]
fn trim_frees_consumed_tuples() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(false, false, 65_536);
    ts.set_eflags(0).unwrap();
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
    let mut peak = 0;
    for round in 0..200 {
        for v in 0..1000 {
            put_i32(&mut ts, &desc, round * 1000 + v);
        }
        for v in 0..1000 {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), round * 1000 + v);
        }
        ts.trim();
        peak = peak.max(ts.0.with_mut(|st| st.tuplecontext.used()));
    }
    assert!(ts.in_memory());
    assert_eq!(ts.tuple_count(), 200_000);
    assert!(peak < 1 << 20, "trimmed tuples must be released: peak {peak}");
    exectuples::exec_clear_tuple(&mut slot, mcx);
    ts.end();
}

#[test]
fn get_stats_tracks_chunk_space_maximum_across_clear() {
    let mcx = leaked_mcx();
    let desc = int4_desc(mcx, 1);
    let mut ts = Tuplestore::begin_heap(true, false, 64);
    // Empty store: memtuples array only (16 KiB of pointers -> aset external chunk; 2048 on 64-bit, 4096 on wasm32).
    let s0 = ts.get_stats();
    assert_eq!(s0.space_type.name(), "Memory");
    assert_eq!(s0.max_space, 16384 + 8);
    for i in 0..10 {
        ts.putvalues(&desc, &[Datum::from_i32(i)], &[false]).unwrap();
    }
    let s1 = ts.get_stats();
    assert!(s1.max_space > s0.max_space);
    ts.clear();
    assert_eq!(ts.get_stats().max_space, s1.max_space);
    ts.end();
}

mod spill {
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Once;

    use super::*;

    static SETUP: Once = Once::new();
    static WAL_SYNC_METHOD: AtomicI32 = AtomicI32::new(0);
    static CWD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn enter_datadir(tag: &str) -> (std::sync::MutexGuard<'static, ()>, String) {
        let guard = CWD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = format!(
            "{}/pgrust-tstorespill-{}-{}",
            std::env::temp_dir().display(),
            std::process::id(),
            tag
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(format!("{dir}/base/pgsql_tmp")).unwrap();
        std::env::set_current_dir(&dir).unwrap();
        (guard, dir)
    }

    fn setup() {
        SETUP.call_once(|| {
            guc_tables::init_seams();
            elog::init_seams();
            fd::init_seams();
            xact_seams::get_current_sub_transaction_id::set(|| 1);
            aio_seams::pgaio_closing_fd::set(|_| {});
            aio_seams::pgaio_io_start_readv::set(|_, _, _| Ok(()));
            waitevent_seams::pgstat_report_wait_start::set(|_| {});
            waitevent_seams::pgstat_report_wait_end::set(|| {});
            pgstat_seams::pgstat_report_tempfile::set(|_| {});
            ipc_seams::before_shmem_exit::set(|_, _| Ok(()));
            ipc_seams::on_shmem_exit::set(|_cb, _arg| {});
            resowner::init_seams();
            guc_tables::vars::wal_sync_method.install(guc_tables::GucVarAccessors {
                get: || WAL_SYNC_METHOD.load(Ordering::Relaxed),
                set: |v| WAL_SYNC_METHOD.store(v, Ordering::Relaxed),
            });
        });
        fd::InitFileAccess();
        let _ = fd::InitTemporaryFileAccess();
        if resowner_seams::current_resource_owner::call().is_null() {
            let owner =
                resowner::ResourceOwnerCreate(types_resowner::ResourceOwner::NULL, "spill-test")
                    .unwrap();
            resowner_seams::set_current_resource_owner::call(owner);
        }
    }

    fn temp_files(dir: &str) -> usize {
        std::fs::read_dir(format!("{dir}/base/pgsql_tmp"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    const N: i32 = 200_000;

    #[test]
    fn try_end_returns_spill_limit_error_without_panicking() {
        setup();
        let (_cwd, dir) = enter_datadir("end-limit");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let saved_owner = resowner::CurrentResourceOwner();
        let owner = resowner::ResourceOwnerCreate(saved_owner, "end-limit").unwrap();
        resowner::SetCurrentResourceOwner(owner);
        let saved_limit = guc_tables::vars::temp_file_limit.read();
        guc_tables::vars::temp_file_limit.write(0);
        let mut ts = Tuplestore::begin_heap(false, false, 0);
        put_i32(&mut ts, &desc, 42);
        assert!(!ts.in_memory());
        let err = ts.try_end().unwrap_err();
        guc_tables::vars::temp_file_limit.write(saved_limit);
        assert_eq!(err.sqlstate(), types_error::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED);
        assert_eq!(err.message(), "temporary file size exceeds \"temp_file_limit\" (0kB)");
        for phase in [
            types_resowner::RESOURCE_RELEASE_BEFORE_LOCKS,
            types_resowner::RESOURCE_RELEASE_LOCKS,
            types_resowner::RESOURCE_RELEASE_AFTER_LOCKS,
        ] {
            resowner::ResourceOwnerRelease(owner, phase, false, false).unwrap();
        }
        resowner::SetCurrentResourceOwner(saved_owner);
        resowner::ResourceOwnerDelete(owner);
        assert_eq!(temp_files(&dir), 0);
    }

    #[test]
    fn spill_keeps_creation_owner_across_child_release_and_pool_reuse() {
        setup();
        let (_cwd, dir) = enter_datadir("owner");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
        let saved = resowner::CurrentResourceOwner();
        for is_commit in [true, false] {
            let owner = resowner::ResourceOwnerCreate(saved, "store-owner").unwrap();
            resowner::SetCurrentResourceOwner(owner);
            let mut ts = Tuplestore::begin_heap(false, false, 0);
            let child = resowner::ResourceOwnerCreate(owner, "spill-child").unwrap();
            resowner::SetCurrentResourceOwner(child);
            put_i32(&mut ts, &desc, 11);
            assert!(!ts.in_memory());
            assert_eq!(resowner::CurrentResourceOwner(), child);
            for phase in [
                types_resowner::RESOURCE_RELEASE_BEFORE_LOCKS,
                types_resowner::RESOURCE_RELEASE_LOCKS,
                types_resowner::RESOURCE_RELEASE_AFTER_LOCKS,
            ] {
                resowner::ResourceOwnerRelease(child, phase, is_commit, false).unwrap();
            }
            resowner::SetCurrentResourceOwner(owner);
            resowner::ResourceOwnerDelete(child);
            let remaining = temp_files(&dir);
            if remaining == 0 {
                std::mem::forget(ts);
                panic!("child release closed the store's spill file");
            }
            put_i32(&mut ts, &desc, 22);
            for expected in [11, 22] {
                assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
                assert_eq!(read_i32(&mut slot), expected);
            }
            assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            exectuples::exec_clear_tuple(&mut slot, mcx);
            ts.end();
            assert_eq!(temp_files(&dir), 0);
            Tuplestore::begin_heap(false, false, 0).end();
            resowner::SetCurrentResourceOwner(saved);
            resowner::ResourceOwnerDelete(owner);
        }
    }

    #[test]
    fn spill_read_scratch_grows_exactly() {
        setup();
        let (_cwd, _dir) = enter_datadir("scratch-growth");
        let mcx = leaked_mcx();
        let mut ts = Tuplestore::begin_heap(false, false, 0);
        for natts in [256, 258] {
            let desc = int4_desc(mcx, natts);
            let values = vec![Datum::from_i32(natts); natts as usize];
            ts.putvalues(&desc, &values, &vec![false; natts as usize]).unwrap();
        }
        assert!(!ts.in_memory());
        for natts in [256, 258] {
            let desc = int4_desc(mcx, natts);
            let mut slot = exectuples::make_tuple_table_slot(
                mcx, TupleSlotKind::MinimalTuple, Some(desc),
            );
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            exectuples::slot_getallattrs(&mut slot);
            for i in 0..natts as usize {
                assert!(!slot.base().tts_isnull[i]);
                assert_eq!(slot.base().tts_values[i].as_i32(), natts);
            }
            ts.0.with_mut(|st| {
                let image = st.scratch_image();
                let required = maxalign(image.len()) / 8;
                assert_eq!(st.read_scratch[st.activeptr].capacity(), required);
                let payload = &image[image.len() - natts as usize * 4..];
                for bytes in payload.chunks_exact(4) {
                    assert_eq!(bytes, natts.to_ne_bytes());
                }
            });
            exectuples::exec_clear_tuple(&mut slot, mcx);
        }
        ts.end();
    }

    #[test]
    fn spill_forward_roundtrip_and_stats() {
        setup();
        let (_cwd, dir) = enter_datadir("fwd");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(false, false, 64);
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory(), "200k tuples must spill at 64KB");
        assert!(temp_files(&dir) > 0);

        for v in 0..N {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }
        assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert!(ts.ateof());

        let stats = ts.get_stats();
        assert!(matches!(
            stats.space_type,
            types_core::instrument::TuplesortSpaceType::Disk
        ));
        assert!(stats.max_space > 0);

        put_i32(&mut ts, &desc, N);
        assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        ts.rescan().unwrap();
        assert!(ts.skiptuples(i64::from(N), true).unwrap());
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), N);

        exectuples::exec_clear_tuple(&mut slot, mcx);
        ts.end();
        assert_eq!(temp_files(&dir), 0, "temp file must be removed at end");
    }

    #[test]
    fn spill_copy_false_reads_do_not_grow_slot_context() {
        setup();
        let (_cwd, _dir) = enter_datadir("noalloc");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let bump: &'static MemoryContext =
            Box::leak(Box::new(MemoryContext::new_bump("ExecutorState-test")));
        let bmcx = bump.mcx();
        let mut slot =
            exectuples::make_tuple_table_slot(bmcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(false, false, 64);
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory(), "200k tuples must spill at 64KB");

        assert!(ts.gettupleslot(true, false, &mut slot, bmcx).unwrap());
        assert_eq!(read_i32(&mut slot), 0);
        let before = bump.used();
        for v in 1..N {
            assert!(ts.gettupleslot(true, false, &mut slot, bmcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }
        assert!(!ts.gettupleslot(true, false, &mut slot, bmcx).unwrap());
        assert_eq!(
            bump.used(),
            before,
            "copy=false reads of a spilled store must not allocate in the slot context"
        );

        ts.rescan().unwrap();
        let mut held =
            exectuples::make_tuple_table_slot(bmcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
        assert!(ts.gettupleslot(true, true, &mut held, bmcx).unwrap());
        assert!(ts.gettupleslot(true, false, &mut slot, bmcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1);
        assert_eq!(read_i32(&mut held), 0);

        exectuples::exec_clear_tuple(&mut held, bmcx);
        exectuples::exec_clear_tuple(&mut slot, bmcx);
        ts.end();
    }

    #[test]
    fn spill_backward_and_rescan() {
        setup();
        let (_cwd, dir) = enter_datadir("back");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(true, false, 64);
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory());

        let mut n = 0;
        while ts.gettupleslot(true, false, &mut slot, mcx).unwrap() {
            n += 1;
        }
        assert_eq!(n, N);

        let mut v = N - 1;
        while ts.gettupleslot(false, false, &mut slot, mcx).unwrap() {
            assert_eq!(read_i32(&mut slot), v);
            v -= 1;
        }
        assert_eq!(v, -1);

        ts.rescan().unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 0);

        assert!(ts.skiptuples(1000, true).unwrap());
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1001);
        assert!(ts.skiptuples(2, false).unwrap());
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1000);

        exectuples::exec_clear_tuple(&mut slot, mcx);
        ts.end();
        assert_eq!(temp_files(&dir), 0);
    }

    #[test]
    fn spill_two_read_pointers_and_copy() {
        setup();
        let (_cwd, _dir) = enter_datadir("ptrs");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(true, false, 64);
        let follower = ts.alloc_read_pointer(EXEC_FLAG_REWIND | EXEC_FLAG_BACKWARD).unwrap();
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory());

        for v in 0..10 {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }
        ts.select_read_pointer(follower).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 0);

        ts.copy_read_pointer(0, follower).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 10);

        ts.select_read_pointer(0).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 10);

        exectuples::exec_clear_tuple(&mut slot, mcx);
        ts.end();
    }

    #[test]
    fn spill_copy_false_rows_survive_another_pointers_reads() {
        setup();
        let (_cwd, _dir) = enter_datadir("ptrscratch");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut outer =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
        let mut inner =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(false, false, 64);
        let second = ts.alloc_read_pointer(EXEC_FLAG_REWIND).unwrap();
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory());

        assert!(ts.gettupleslot(true, false, &mut outer, mcx).unwrap());
        ts.select_read_pointer(second).unwrap();
        for v in 0..5 {
            assert!(ts.gettupleslot(true, false, &mut inner, mcx).unwrap());
            assert_eq!(read_i32(&mut inner), v);
        }
        assert_eq!(read_i32(&mut outer), 0);
        ts.select_read_pointer(0).unwrap();
        assert!(ts.gettupleslot(true, false, &mut outer, mcx).unwrap());
        assert_eq!(read_i32(&mut outer), 1);
        assert_eq!(read_i32(&mut inner), 4);

        exectuples::exec_clear_tuple(&mut outer, mcx);
        exectuples::exec_clear_tuple(&mut inner, mcx);
        ts.end();
    }

    #[test]
    fn spill_window_buffer_multi_pointer_backward_config() {
        setup();
        let (_cwd, dir) = enter_datadir("winbuf");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(false, false, 64);
        ts.set_eflags(0).unwrap();
        let agg = ts.alloc_read_pointer(EXEC_FLAG_BACKWARD).unwrap();
        let func = ts.alloc_read_pointer(EXEC_FLAG_BACKWARD).unwrap();
        let head = ts.alloc_read_pointer(0).unwrap();
        let tail = ts.alloc_read_pointer(0).unwrap();
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory(), "200k tuples must spill at 64KB");
        assert!(temp_files(&dir) > 0);

        for v in 0..1_000 {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }

        ts.select_read_pointer(tail).unwrap();
        assert!(ts.skiptuples(5_000, true).unwrap());
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 5_000);
        ts.select_read_pointer(head).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 0);

        ts.select_read_pointer(agg).unwrap();
        assert!(ts.skiptuples(500, true).unwrap());
        for v in 500..510 {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }
        assert!(ts.skiptuples(10, false).unwrap());
        for v in 500..510 {
            assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
            assert_eq!(read_i32(&mut slot), v);
        }
        assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 508);
        assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 507);
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 508);

        ts.select_read_pointer(0).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1_000);
        ts.select_read_pointer(tail).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 5_001);
        ts.select_read_pointer(head).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1);

        ts.select_read_pointer(func).unwrap();
        assert!(ts.skiptuples(i64::from(N), true).unwrap());
        assert!(!ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), N - 1);
        assert!(ts.gettupleslot(false, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), N - 2);

        ts.copy_read_pointer(0, agg).unwrap();
        ts.select_read_pointer(agg).unwrap();
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 1_001);

        exectuples::exec_clear_tuple(&mut slot, mcx);
        ts.end();
        assert_eq!(temp_files(&dir), 0, "temp file must be removed at end");
    }

    #[test]
    fn spill_clear_returns_to_memory() {
        setup();
        let (_cwd, dir) = enter_datadir("clear");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));

        let mut ts = Tuplestore::begin_heap(false, false, 64);
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory());
        ts.clear();
        assert!(ts.in_memory());
        assert_eq!(temp_files(&dir), 0, "clear must drop the temp file");
        assert_eq!(ts.tuple_count(), 0);

        put_i32(&mut ts, &desc, 7);
        assert!(ts.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 7);

        let stats = ts.get_stats();
        assert!(matches!(
            stats.space_type,
            types_core::instrument::TuplesortSpaceType::Disk
        ));

        exectuples::exec_clear_tuple(&mut slot, mcx);
        ts.end();
    }

    /// D2620: an abort's resource-owner release closes a spilled store's file under it (a failed portal's cursor
    /// store); ending the store afterwards (PortalDrop at the COMMIT / ROLLBACK) must not ask that freed file its size.
    #[test]
    fn spill_end_after_abort_release_freed_the_file() {
        setup();
        let (_cwd, dir) = enter_datadir("released");
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let saved = resowner::CurrentResourceOwner();
        let owner = resowner::ResourceOwnerCreate(saved, "portal").unwrap();
        resowner::SetCurrentResourceOwner(owner);
        let mut ts = Tuplestore::begin_heap(false, false, 64);
        for v in 0..N {
            put_i32(&mut ts, &desc, v);
        }
        assert!(!ts.in_memory());
        resowner::SetCurrentResourceOwner(saved);
        for phase in [
            types_resowner::RESOURCE_RELEASE_BEFORE_LOCKS,
            types_resowner::RESOURCE_RELEASE_LOCKS,
            types_resowner::RESOURCE_RELEASE_AFTER_LOCKS,
        ] {
            resowner::ResourceOwnerRelease(owner, phase, false, false).unwrap();
        }
        assert_eq!(temp_files(&dir), 0, "the abort release closes the spill file");
        ts.end();
        resowner::ResourceOwnerDelete(owner);
        // The pool hands out a sound store afterwards.
        let mut again = Tuplestore::begin_heap(false, false, 64);
        put_i32(&mut again, &desc, 5);
        let mut slot =
            exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(desc.clone()));
        assert!(again.gettupleslot(true, false, &mut slot, mcx).unwrap());
        assert_eq!(read_i32(&mut slot), 5);
        exectuples::exec_clear_tuple(&mut slot, mcx);
        again.end();
    }
}

mod guards {
    use ::types_error::{ERRCODE_INTERNAL_ERROR, ERRCODE_UNDEFINED_FILE};

    use super::*;

    #[test]
    fn late_eflags_changes_are_errors_not_panics() {
        let mcx = leaked_mcx();
        let desc = int4_desc(mcx, 1);
        let mut ts = Tuplestore::begin_heap(false, false, 1024);
        ts.set_eflags(0).unwrap();
        assert_eq!(ts.alloc_read_pointer(0).unwrap(), 1);
        put_i32(&mut ts, &desc, 1);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ts.set_eflags(EXEC_FLAG_BACKWARD)
        }));
        let err = outcome
            .expect("tuplestore_set_eflags after a put must elog(ERROR), not panic")
            .expect_err("tuplestore_set_eflags after a put must fail");
        assert_eq!(err.message(), "too late to call tuplestore_set_eflags");
        assert_eq!(err.sqlstate(), ERRCODE_INTERNAL_ERROR);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ts.alloc_read_pointer(EXEC_FLAG_BACKWARD)
        }));
        let err = outcome
            .expect("alloc_read_pointer widening eflags after a put must elog(ERROR), not panic")
            .expect_err("widening the store's eflags after a put must fail");
        assert_eq!(err.message(), "too late to require new tuplestore eflags");
        assert_eq!(err.sqlstate(), ERRCODE_INTERNAL_ERROR);
        assert_eq!(ts.alloc_read_pointer(0).unwrap(), 2);
        ts.end();
    }

    #[test]
    fn seek_failure_carries_the_file_access_sqlstate() {
        let missing = std::fs::File::open("/nonexistent/pgrust-b054-seek-witness");
        assert!(missing.is_err());
        let errno_now = ::elog::errno::current_errno();
        let err = crate::seek_failed();
        assert_eq!(err.message(), "could not seek in tuplestore temporary file");
        assert_eq!(err.sqlstate(), ::elog::errno::sqlstate_for_file_access(errno_now));
        assert_eq!(err.sqlstate(), ERRCODE_UNDEFINED_FILE);
        assert_ne!(err.sqlstate(), ERRCODE_INTERNAL_ERROR);
    }
}

#[test]
fn initial_memtuples_is_c_formula_for_the_pointer_width() {
    // C: Max(16384 / sizeof(void *), ALLOCSET_SEPARATE_THRESHOLD / sizeof(void *) + 1); D3255: a 64-bit constant
    // (2048) was half of C's array on wasm32, so EXPLAIN ANALYZE's "Maximum Storage" read 9kB where PostgreSQL reads
    // 17kB.
    for (ptr, want) in [(8usize, 2048usize), (4, 4096)] {
        assert_eq!((16384 / ptr).max(8192 / ptr + 1), want);
    }
    assert_eq!(INITIAL_MEMTUPSIZE * PTR_SIZE, 16384);
    assert_eq!(INITIAL_MEMTUPSIZE, (16384 / PTR_SIZE).max(ALLOCSET_SEPARATE_THRESHOLD / PTR_SIZE + 1));
}
