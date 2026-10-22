use super::*;

use std::sync::atomic::{AtomicI32, AtomicU32};
use std::sync::Once;

use types_storage::aio::{
    PgAioResultStatus, PGAIO_HCB_INVALID, PGAIO_HCB_LOCAL_BUFFER_READV, PGAIO_HF_BUFFERED,
    PGAIO_OP_READV, PGAIO_TID_SMGR,
};

#[test]
fn staged_debug_report_matches_c() {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    fn hook(error: &types_error::PgError, output: &mut bool) {
        let n = SEEN.fetch_add(1, Ordering::Relaxed);
        assert_eq!(error.level, types_error::DEBUG3);
        assert_eq!(error.message, format!(
            "io 7         |op readv|target smgr|state STAGED          : staged (synchronous: {}, in_batch: {})",
            n / 2, n % 2,
        ));
        assert!(error.hide_statement);
        assert!(error.hide_context);
        assert_eq!(
            error.location,
            Some(types_error::ErrorLocation::new("aio.c", 459, "pgaio_io_stage")),
        );
        *output = false;
    }

    let h = new_handle(0, 0);
    // SAFETY: this handle is private to this test.
    unsafe {
        let d = h.data();
        d.op = PGAIO_OP_READV;
        d.target = PGAIO_TID_SMGR;
    }
    h.set_state(PGAIO_HS_STAGED);
    let old_level = elog::config::log_min_messages();
    let old_client = elog::config::client_min_messages();
    let old_hook = elog::set_emit_log_hook(Some(hook));
    elog::config::set_client_min_messages(types_error::ERROR);
    elog::config::set_log_min_messages(types_error::DEBUG3);
    for synchronous in [false, true] {
        for in_batch in [false, true] {
            handle::report_staged(7, &h, synchronous, in_batch).unwrap();
        }
    }
    elog::config::set_log_min_messages(types_error::DEBUG2);
    handle::report_staged(7, &h, true, true).unwrap();
    elog::set_emit_log_hook(old_hook);
    elog::config::set_log_min_messages(old_level);
    elog::config::set_client_min_messages(old_client);
    assert_eq!(SEEN.load(Ordering::Relaxed), 4);
}

#[test]
fn check_io_method_accepts_sync_and_worker_refuses_others() {
    let mut extra = None;
    for v in [guc_tables::consts::IOMETHOD_SYNC, guc_tables::consts::IOMETHOD_WORKER] {
        let mut v = v;
        assert!(check_io_method(&mut v, &mut extra, types_guc::GucSource::PGC_S_TEST).unwrap());
    }
    let mut v = 2; // io_uring's C value: unported until inc-2
    assert!(!check_io_method(&mut v, &mut extra, types_guc::GucSource::PGC_S_TEST).unwrap());
}

#[test]
fn check_io_max_concurrency_bounds() {
    let mut extra = None;
    let mut v = -1;
    assert!(check_io_max_concurrency(&mut v, &mut extra, types_guc::GucSource::PGC_S_TEST).unwrap());
    let mut v = 0;
    assert!(!check_io_max_concurrency(&mut v, &mut extra, types_guc::GucSource::PGC_S_TEST).unwrap());
    let mut v = 7;
    assert!(check_io_max_concurrency(&mut v, &mut extra, types_guc::GucSource::PGC_S_TEST).unwrap());
}

#[test]
fn worker_submission_queue_lock_offset_is_pinned() {
    assert_eq!(
        lwlock::GetLWTrancheName(method_worker::AIO_WORKER_SUBMISSION_QUEUE_LOCK as u16),
        "AioWorkerSubmissionQueue"
    );
}

#[test]
fn wref_roundtrip() {
    let w = types_storage::buf::PgAioWaitRef {
        aio_index: 7,
        generation_upper: 1,
        generation_lower: 0xffff_fffe,
    };
    assert!(pgaio_wref_valid(&w));
    let mut w2 = w;
    pgaio_wref_clear(&mut w2);
    assert!(!pgaio_wref_valid(&w2));
}

// Boot-time geometry laid out the way AioShmemInit does it, in leaked heap
// memory (no shmem segment in unit tests): 3 backends x 8 handles = 24
// handles, io_max_combine_limit 4. Installed once per test process; tests
// touch disjoint handles.
const TEST_PROCS: usize = 3;
const TEST_IMC: usize = 8;
const TEST_COMBINE: usize = 4;
static TEST_COMBINE_LIMIT: AtomicI32 = AtomicI32::new(TEST_COMBINE as i32);

pub(super) fn test_ctl() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        guc_tables::vars::io_max_combine_limit.install_if_absent(GucVarAccessors {
            get: || TEST_COMBINE_LIMIT.load(Ordering::Relaxed),
            set: |v| TEST_COMBINE_LIMIT.store(v, Ordering::Relaxed),
        });
        let per_backend_iovecs = TEST_IMC * TEST_COMBINE;
        let mut backends = Vec::with_capacity(TEST_PROCS);
        let mut handles = Vec::with_capacity(TEST_PROCS * TEST_IMC);
        let mut io_handle_off: u32 = 0;
        let mut iovec_off: u32 = 0;
        for procno in 0..TEST_PROCS {
            backends.push(new_backend(io_handle_off));
            for _ in 0..TEST_IMC {
                handles.push(new_handle(procno as i32, iovec_off));
                iovec_off += TEST_COMBINE as u32;
            }
            io_handle_off += TEST_IMC as u32;
        }
        let iovecs = vec![
            libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
            TEST_PROCS * per_backend_iovecs
        ];
        let handle_data = vec![0u64; TEST_PROCS * per_backend_iovecs];
        let ctl = Box::new(PgAioCtl {
            io_handle_count: (TEST_PROCS * TEST_IMC) as u64,
            iovec_count: (TEST_PROCS * per_backend_iovecs) as u64,
            backend_count: TEST_PROCS as u64,
            backend_state: Box::leak(backends.into_boxed_slice()).as_mut_ptr(),
            io_handles: Box::leak(handles.into_boxed_slice()).as_mut_ptr(),
            iovecs: Box::leak(iovecs.into_boxed_slice()).as_mut_ptr(),
            handle_data: Box::leak(handle_data.into_boxed_slice()).as_mut_ptr(),
        });
        publish_ctl(Box::leak(ctl));
        for procno in 0..TEST_PROCS {
            let slot = backend_slot(procno as i32);
            // SAFETY: single-threaded table bring-up.
            let b = unsafe { &mut *slot.b.get() };
            for i in 0..TEST_IMC {
                dclist_push_tail(&mut b.idle_ios, slot.io_handle_off + i as u32);
            }
        }
    });
}

#[test]
fn io_handles_per_backend_ignores_clobbered_guc() {
    test_ctl();

    // Simulate child-launch GUC republication clobbering io_max_concurrency
    // back to its -1 sentinel (which becomes u32::MAX when cast).
    IO_MAX_CONCURRENCY.store(-1, Ordering::Relaxed);

    // The scan bound used by pgaio_io_wait_for_free must come from the
    // immutable table geometry (pgaio_ctl), NOT the live GUC.
    assert_eq!(io_handles_per_backend(), TEST_IMC);
    assert_eq!(pgaio_io_handle_count(), TEST_PROCS * TEST_IMC);
    assert_ne!(io_handles_per_backend() as i64, io_max_concurrency() as i64);
}

// aio_funcs.c pg_get_aios: a non-IDLE handle of ANY backend is rendered from
// a consistent copy (state/generation re-checked), IDLE ones are skipped.
#[test]
fn pgaio_io_snapshot_renders_a_non_idle_handle_of_another_backend() {
    test_ctl();
    // No PGPROC table in this harness: inject the owner-pid read.
    let snap = |index: u32| pgaio_io_snapshot_with(index, |_| 0);
    let index: u32 = (1 * TEST_IMC + 5) as u32; // procno 1, its 6th handle
    let h = ioh(index);
    assert_eq!(h.owner_procno, 1);
    assert!(snap(index).is_none(), "IDLE handles are not displayed");

    // SAFETY: test-owned handle, no concurrent access.
    unsafe {
        let d = h.data();
        d.op = PGAIO_OP_READV;
        d.op_data.offset = 8192;
        d.op_data.iov_length = 2;
        d.target = PGAIO_TID_SMGR;
        d.target_data.smgr.blockNum = 7;
        d.target_data.smgr.nblocks = 3;
        d.handle_data_len = 2;
        d.distilled_result.status = PgAioResultStatus::Unknown;
        let iov = iovec_region(h.iovec_off);
        (*iov).iov_len = 8192;
        (*iov.add(1)).iov_len = 16384;
    }
    h.flags.store(PGAIO_HF_BUFFERED, Ordering::Relaxed);
    h.set_state(PGAIO_HS_SUBMITTED);

    let s = snap(index).expect("SUBMITTED handle is displayed");
    assert_eq!(s.id, index as i32);
    assert_eq!(s.generation, 1);
    assert_eq!(s.state, PGAIO_HS_SUBMITTED);
    assert_eq!(pgaio_io_state_name(s.state), "SUBMITTED");
    assert_eq!(s.owner_procno, 1);
    assert_eq!(pgaio_io_op_name(s.op), "readv");
    assert_eq!(s.op_offset, 8192);
    assert_eq!(s.iov_bytes, 8192 + 16384, "iov_byte_length over iov_length entries");
    assert_eq!(pgaio_io_target_name(s.target), "smgr");
    assert_eq!(s.handle_data_len, 2);
    assert_eq!(pgaio_result_status_string(s.distilled_status), "UNKNOWN");
    assert_eq!(s.target_data.smgr.blockNum, 7);
    assert_eq!(s.target_data.smgr.nblocks, 3);
    assert_eq!(s.flags & PGAIO_HF_BUFFERED, PGAIO_HF_BUFFERED);

    // aio_funcs.c:113-117: the owner pid is read BEFORE the generation
    // re-check, so an owner that exited (its IO waited for, the handle
    // recycled) is dropped by that check instead of reported with a stale
    // or reused pid.
    let s = pgaio_io_snapshot_with(index, |procno| {
        assert_eq!(procno, 1);
        4242
    })
    .expect("SUBMITTED handle is displayed");
    assert_eq!(s.owner_pid, 4242);
    assert!(
        pgaio_io_snapshot_with(index, |_| {
            h.generation.fetch_add(1, Ordering::Relaxed);
            0
        })
        .is_none(),
        "a generation change after the pid read drops the handle"
    );
    assert_eq!(snap(index).expect("still SUBMITTED").generation, 2);

    // A later generation is just a later IO: still displayed while non-IDLE
    // (C only drops a handle whose generation moved DURING its copy).
    h.generation.fetch_add(1, Ordering::Relaxed);
    assert_eq!(snap(index).expect("still SUBMITTED").generation, 3);

    h.set_state(PGAIO_HS_IDLE);
    assert!(snap(index).is_none());
}

// aio_callback.c:289/330: local completion callbacks run inside a critical
// section (a failure there is a PANIC, never a torn unwind).
static LOCAL_COMPLETE_SEEN_CRIT: AtomicU32 = AtomicU32::new(u32::MAX);

#[test]
fn complete_local_runs_its_callbacks_inside_a_critical_section() {
    test_ctl();
    bufmgr_seams::aio_local_buffer_readv_complete::set(|_ioh, result, _cb_data| {
        LOCAL_COMPLETE_SEEN_CRIT
            .store(init_small::globals::CritSectionCount(), Ordering::Relaxed);
        result
    });

    let index: u32 = (2 * TEST_IMC + 1) as u32; // procno 2, its 2nd handle
    let h = ioh(index);
    // SAFETY: test-owned handle, no concurrent access.
    unsafe {
        let d = h.data();
        d.num_callbacks = 1;
        d.callbacks[0] = PGAIO_HCB_LOCAL_BUFFER_READV;
        d.callbacks_data[0] = 0;
        d.distilled_result = PgAioResult {
            status: PgAioResultStatus::Ok,
            result: 8192,
            id: PGAIO_HCB_LOCAL_BUFFER_READV,
            error_data: 0,
        };
    }
    h.set_state(PGAIO_HS_COMPLETED_SHARED);

    assert_eq!(init_small::globals::CritSectionCount(), 0);
    let r = callback::pgaio_io_call_complete_local(index);
    assert_eq!(r.status, PgAioResultStatus::Ok);
    assert_ne!(LOCAL_COMPLETE_SEEN_CRIT.load(Ordering::Relaxed), u32::MAX, "callback ran");
    assert!(
        LOCAL_COMPLETE_SEEN_CRIT.load(Ordering::Relaxed) > 0,
        "complete_local callback must run with CritSectionCount > 0 (START_CRIT_SECTION)"
    );
    assert_eq!(init_small::globals::CritSectionCount(), 0, "END_CRIT_SECTION restores");

    h.set_state(PGAIO_HS_IDLE);
}

// aio_callback.c:182: elog(ERROR, "callback %d/%s does not have report callback").
#[test]
fn result_report_without_report_callback_names_the_callback_like_c() {
    let err = pgaio_result_report(
        PgAioResult {
            status: PgAioResultStatus::Error,
            result: 0,
            id: PGAIO_HCB_INVALID,
            error_data: 0,
        },
        &PgAioTargetData::default(),
        types_error::ERROR,
    )
    .expect_err("no report callback");
    assert_eq!(err.message, "callback 0/aio_invalid_cb does not have report callback");
}

// method_worker.c:120-158: the worker method reserves and registers its
// submission queue + control block in shared memory (not process statics).
#[test]
fn worker_method_reserves_shmem_for_queue_and_control() {
    assert!(method_worker::pgaio_worker_shmem_size() > 0);
}

// aio_init.c:142/213 + method_sync.c:28-31: only the selected method's
// shmem callbacks run, and the sync method has none, so io_method=sync
// reserves no AioWorkerSubmissionQueue/AioWorkerControl.
#[test]
fn only_the_selected_method_reserves_shmem() {
    assert_eq!(init::pgaio_method_shmem_size(IOMETHOD_SYNC), 0);
    assert_eq!(
        init::pgaio_method_shmem_size(IOMETHOD_WORKER),
        method_worker::pgaio_worker_shmem_size()
    );
}

// aio.c pgaio_io_state_get_name / aio_io.c pgaio_io_get_op_name /
// aio_target.c target names / aio.c pgaio_result_status_string.
#[test]
fn name_tables_match_c() {
    assert_eq!(pgaio_io_state_name(PGAIO_HS_IDLE), "IDLE");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_HANDED_OUT), "HANDED_OUT");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_DEFINED), "DEFINED");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_STAGED), "STAGED");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_SUBMITTED), "SUBMITTED");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_COMPLETED_IO), "COMPLETED_IO");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_COMPLETED_SHARED), "COMPLETED_SHARED");
    assert_eq!(pgaio_io_state_name(PGAIO_HS_COMPLETED_LOCAL), "COMPLETED_LOCAL");
    assert_eq!(pgaio_io_op_name(types_storage::aio::PGAIO_OP_INVALID), "invalid");
    assert_eq!(pgaio_io_op_name(PGAIO_OP_READV), "readv");
    assert_eq!(pgaio_io_op_name(types_storage::aio::PGAIO_OP_WRITEV), "writev");
    assert_eq!(pgaio_io_target_name(types_storage::aio::PGAIO_TID_INVALID), "invalid");
    assert_eq!(pgaio_io_target_name(PGAIO_TID_SMGR), "smgr");
    assert_eq!(pgaio_result_status_string(PgAioResultStatus::Unknown), "UNKNOWN");
    assert_eq!(pgaio_result_status_string(PgAioResultStatus::Ok), "OK");
    assert_eq!(pgaio_result_status_string(PgAioResultStatus::Warning), "WARNING");
    assert_eq!(pgaio_result_status_string(PgAioResultStatus::Partial), "PARTIAL");
    assert_eq!(pgaio_result_status_string(PgAioResultStatus::Error), "ERROR");
}

// aio_init.c:227: elog(ERROR, "aio requires a normal PGPROC") — a catchable
// error, not a thread panic (the unit harness has no MyProc).
#[test]
fn init_backend_without_a_pgproc_is_a_catchable_error() {
    assert!(lmgr_proc::MyProc().is_none());
    let err = pgaio_init_backend().expect_err("no PGPROC");
    assert_eq!(err.message, "aio requires a normal PGPROC");
    assert!(MY_BACKEND.get().is_none());
}

#[test]
fn callback_debug_messages_match_c() {
    static SEEN: std::sync::Mutex<Vec<types_error::PgError>> = std::sync::Mutex::new(Vec::new());
    test_ctl();
    let index = 22;
    let h = ioh(index);
    h.set_state(PGAIO_HS_HANDED_OUT);
    let previous_level = elog::config::log_min_messages();
    elog::config::set_log_min_messages(types_error::DEBUG4);
    let previous_hook = elog::set_emit_log_hook(Some(|error, output| {
        SEEN.lock().unwrap().push(error.clone());
        *output = false;
    }));
    bufmgr_seams::aio_buffer_readv_stage::set(|_, _, _| {});
    smgr_seams::aio_md_readv_complete::set(|_, mut result, _| {
        result.result /= 8192;
        result
    });
    bufmgr_seams::aio_shared_buffer_readv_complete::set(|_, result, _| result);
    bufmgr_seams::aio_shared_buffer_readv_complete_local::set(|_, result, _| result);
    pgaio_io_register_callbacks(index, types_storage::aio::PGAIO_HCB_SHARED_BUFFER_READV, 7);
    pgaio_io_set_target_smgr(
        index,
        types_storage::RelFileLocator::default(),
        types_core::ForkNumber::MAIN_FORKNUM,
        0,
        1,
        false,
        false,
    );
    pgaio_io_register_callbacks(index, types_storage::aio::PGAIO_HCB_MD_READV, 0);
    // SAFETY: this test exclusively owns handle 23.
    unsafe { h.data() }.op = PGAIO_OP_READV;
    h.set_state(PGAIO_HS_DEFINED);
    callback::pgaio_io_call_stage(index);
    h.result.store(8192, Ordering::Relaxed);
    h.set_state(PGAIO_HS_COMPLETED_IO);
    init_small::globals::StartCriticalSection();
    callback::pgaio_io_call_complete_shared(index);
    init_small::globals::EndCriticalSection();
    h.set_state(PGAIO_HS_COMPLETED_SHARED);
    callback::pgaio_io_call_complete_local(index);
    elog::set_emit_log_hook(previous_hook);
    elog::config::set_log_min_messages(previous_level);
    h.set_state(PGAIO_HS_IDLE);

    let seen = SEEN.lock().unwrap();
    let expected = [
        (types_error::DEBUG3, 106, "pgaio_io_register_callbacks", "invalid", "invalid", "HANDED_OUT", "adding cb #1, id 2/aio_shared_buffer_readv_cb"),
        (types_error::DEBUG3, 106, "pgaio_io_register_callbacks", "invalid", "smgr", "HANDED_OUT", "adding cb #2, id 1/aio_md_readv_cb"),
        (types_error::DEBUG3, 215, "pgaio_io_call_stage", "readv", "smgr", "DEFINED", "calling cb #1 2/aio_shared_buffer_readv_cb->stage(7)"),
        (types_error::DEBUG4, 257, "pgaio_io_call_complete_shared", "readv", "smgr", "COMPLETED_IO", "calling cb #2, id 1/aio_md_readv_cb->complete_shared(0) with distilled result: (status OK, id 0, error_data 0, result 8192)"),
        (types_error::DEBUG4, 257, "pgaio_io_call_complete_shared", "readv", "smgr", "COMPLETED_IO", "calling cb #1, id 2/aio_shared_buffer_readv_cb->complete_shared(7) with distilled result: (status OK, id 0, error_data 0, result 1)"),
        (types_error::DEBUG3, 270, "pgaio_io_call_complete_shared", "readv", "smgr", "COMPLETED_IO", "after shared completion: distilled result: (status OK, id 0, error_data: 0, result 1), raw_result: 8192"),
        (types_error::DEBUG4, 311, "pgaio_io_call_complete_local", "readv", "smgr", "COMPLETED_SHARED", "calling cb #1, id 2/aio_shared_buffer_readv_cb->complete_local(7) with distilled result: status OK, id 0, error_data 0, result 1"),
        (types_error::DEBUG3, 328, "pgaio_io_call_complete_local", "readv", "smgr", "COMPLETED_SHARED", "after local completion: result: (status OK, id 0, error_data 0, result 1), raw_result: 8192"),
    ];
    assert_eq!(seen.len(), expected.len());
    for (error, (level, line, function, op, target, state, message)) in seen.iter().zip(expected) {
        assert_eq!(error.level(), level);
        assert_eq!(
            error.message(),
            format!("io 22        |op {op:<5}|target {target:<4}|state {state:<16}: {message}")
        );
        assert!(error.hide_statement());
        assert!(error.hide_context());
        assert_eq!(
            error.location,
            Some(types_error::ErrorLocation::new(
                "aio_callback.c",
                line,
                function
            ))
        );
    }
}

#[test]
fn update_state_debug_report_matches_c() {
    static SEEN: std::sync::Mutex<Vec<types_error::PgError>> = std::sync::Mutex::new(Vec::new());
    test_ctl();
    let index: u32 = 20;
    let h = ioh(index);
    h.set_state(PGAIO_HS_IDLE);
    let previous_level = elog::config::log_min_messages();
    let previous_client = elog::config::client_min_messages();
    elog::config::set_client_min_messages(types_error::ERROR);
    elog::config::set_log_min_messages(types_error::DEBUG5);
    let previous_hook = elog::set_emit_log_hook(Some(|error, output| {
        SEEN.lock().unwrap().push(error.clone());
        *output = false;
    }));
    handle::update_state(index, PGAIO_HS_HANDED_OUT);
    handle::aio_debug(types_error::DEBUG4, 1145, "pgaio_submit_staged", || {
        "aio: submitted 1 IOs".to_string()
    });
    elog::config::set_log_min_messages(types_error::DEBUG1);
    handle::update_state(index, PGAIO_HS_IDLE);
    elog::set_emit_log_hook(previous_hook);
    elog::config::set_log_min_messages(previous_level);
    elog::config::set_client_min_messages(previous_client);

    let seen = SEEN.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].level(), types_error::DEBUG5);
    assert_eq!(
        seen[0].message(),
        "io 20        |op invalid|target invalid|state IDLE            : updating state to HANDED_OUT"
    );
    assert!(seen[0].hide_statement() && seen[0].hide_context());
    assert_eq!(
        seen[0].location,
        Some(types_error::ErrorLocation::new("aio.c", 395, "pgaio_io_update_state"))
    );
    assert_eq!(seen[1].message(), "aio: submitted 1 IOs");
    assert_eq!(h.state(), PGAIO_HS_IDLE);
}
