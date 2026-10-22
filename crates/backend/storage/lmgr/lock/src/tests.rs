use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering::SeqCst};
use std::sync::{Condvar, Mutex, Once, OnceLock};

use init_small::globals as g;
use types_core::BackendType;
use types_error::{ErrorLevel, PgError};
use types_storage::lock::{
    AccessExclusiveLock, AccessShareLock, DeadLockState, ExclusiveLock, RowExclusiveLock,
    ShareLock, LOCALLOCKTAG, LOCKACQUIRE_ALREADY_CLEAR, LOCKACQUIRE_ALREADY_HELD, LOCKACQUIRE_NOT_AVAIL,
    LOCKACQUIRE_OK, LOCKTAG,
};

const TESTDB: u32 = 7777;
const MAX_CONNECTIONS: i32 = 32;
const MAX_WORKER_PROCESSES: i32 = 2;
const MAX_BACKENDS: i32 = MAX_CONNECTIONS + 3 + MAX_WORKER_PROCESSES + 2 + 2;

const CFG: lmgr_proc::ProcGlobalConfig = lmgr_proc::ProcGlobalConfig {
    autovacuum_worker_slots: 3,
    max_wal_senders: 2,
    max_prepared_xacts: 2,
    fastpath_lock_groups_per_backend: 1,
};

static NEXT_PID: AtomicI32 = AtomicI32::new(9100);

fn logged() -> &'static Mutex<Vec<(ErrorLevel, String, Option<String>)>> {
    static LOGGED: OnceLock<Mutex<Vec<(ErrorLevel, String, Option<String>)>>> = OnceLock::new();
    LOGGED.get_or_init(|| Mutex::new(Vec::new()))
}

static DEADLOCK_ALERT_PENDING: AtomicI32 = AtomicI32::new(-1);
static BLOCKED_BY_AUTOVAC: AtomicI32 = AtomicI32::new(-1);
static AUTOVAC_BLOCKER: AtomicI32 = AtomicI32::new(-1);
static SIGNAL_ERRNO: AtomicI32 = AtomicI32::new(0);
// Nonzero so a stale PGPROC.waitStart is distinguishable from a cleared one.
const TEST_WAIT_START: i64 = 1_234_567;

fn semas() -> &'static (Mutex<HashMap<types_core::ProcNumber, i32>>, Condvar) {
    static SEMS: OnceLock<(Mutex<HashMap<types_core::ProcNumber, i32>>, Condvar)> =
        OnceLock::new();
    SEMS.get_or_init(|| (Mutex::new(HashMap::new()), Condvar::new()))
}

fn thread_globals() {
    g::SetMaxConnections(MAX_CONNECTIONS);
    g::set_max_worker_processes(MAX_WORKER_PROCESSES);
    g::SetMaxBackends(MAX_BACKENDS);
    g::SetMyProcPid(NEXT_PID.fetch_add(1, SeqCst));
    g::SetMyDatabaseId(TESTDB);
}

fn my_latch_wait() {
    let procno = lmgr_proc::MyProc().expect("waiting without a proc");
    let latch = &lmgr_proc::GetPGProcByNumber(procno).procLatch;
    while latch.is_set.load(SeqCst) == 0 {
        std::thread::yield_now();
    }
}

fn setup() {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        thread_globals();

        pg_sema_seams::pg_semaphore_create::set(|procno| {
            semas().0.lock().unwrap().insert(procno, 0);
        });
        pg_sema_seams::pg_semaphore_reset::set(|procno| {
            semas().0.lock().unwrap().insert(procno, 0);
        });
        pg_sema_seams::pg_semaphore_lock::set(|procno| {
            let (map, cv) = semas();
            let mut counts = map.lock().unwrap();
            loop {
                let count = counts.get_mut(&procno).unwrap();
                if *count > 0 {
                    *count -= 1;
                    return;
                }
                counts = cv.wait(counts).unwrap();
            }
        });
        pg_sema_seams::pg_semaphore_unlock::set(|procno| {
            let (map, cv) = semas();
            *map.lock().unwrap().get_mut(&procno).unwrap() += 1;
            cv.notify_all();
        });

        postgres_seams::check_for_interrupts::set(|| Ok(()));
        s_lock_seams::perform_spin_delay::set(|_| std::thread::yield_now());
        s_lock_seams::finish_spin_delay::set(|_| {});
        s_lock_seams::set_spins_per_delay::set(|_| {});
        s_lock_seams::update_spins_per_delay::set(|v| v);

        latch_seams::own_latch::set(|latch| latch.owner_pid.store(g::MyProcPid(), SeqCst));
        latch_seams::disown_latch::set(|latch| latch.owner_pid.store(0, SeqCst));
        latch_seams::set_latch::set(|latch| latch.is_set.store(1, SeqCst));
        latch_seams::set_latch_my_latch::set(|| {
            if let Some(procno) = lmgr_proc::MyProc() {
                lmgr_proc::GetPGProcByNumber(procno)
                    .procLatch
                    .is_set
                    .store(1, SeqCst);
            }
        });
        latch_seams::wait_latch_my_latch::set(|_, _, _| {
            let me = lmgr_proc::MyProc().unwrap_or(-1);
            if me >= 0 && DEADLOCK_ALERT_PENDING.compare_exchange(me, -1, SeqCst, SeqCst).is_ok()
            {
                lmgr_proc::CheckDeadLockAlert();
                return types_storage::waiteventset::WL_LATCH_SET;
            }
            my_latch_wait();
            types_storage::waiteventset::WL_LATCH_SET
        });
        latch_seams::reset_latch_my_latch::set(|| {
            let procno = lmgr_proc::MyProc().expect("reset without a proc");
            lmgr_proc::GetPGProcByNumber(procno)
                .procLatch
                .is_set
                .store(0, SeqCst);
        });
        miscinit_seams::switch_to_shared_latch::set(|| {});
        miscinit_seams::switch_back_to_local_latch::set(|| {});
        waitevent_seams::pgstat_set_wait_event_storage::set(|_| {});
        waitevent_seams::pgstat_reset_wait_event_storage::set(|| {});
        waitevent_seams::pgstat_report_wait_start::set(|_| {});
        waitevent_seams::pgstat_report_wait_end::set(|| {});
        ipc_seams::on_shmem_exit::set(|_, _| {});
        pmsignal_seams::register_postmaster_child_active::set(|| {});

        deadlock_seams::init_dead_lock_checking::set(|| Ok(()));
        deadlock_seams::dead_lock_check::set(|procno| {
            if procno == BLOCKED_BY_AUTOVAC.load(SeqCst) {
                DeadLockState::BlockedByAutoVacuum
            } else {
                DeadLockState::NoDeadLock
            }
        });
        deadlock_seams::dead_lock_report::set(|| {
            Err(Box::new(PgError::new(
                types_error::ERROR,
                "deadlock detected",
            )))
        });
        deadlock_seams::remember_simple_deadlock::set(|_, _, _, _| {});
        deadlock_seams::get_blocking_autovacuum_procno::set(|| {
            let p = AUTOVAC_BLOCKER.load(SeqCst);
            (p >= 0).then_some(p)
        });

        resowner::init_seams();

        transam_xlog_seams::recovery_in_progress::set(|| false);
        transam_xlog_seams::xlog_standby_info_active::set(|| false);

        timeout_seams::enable_timeout_after::set(|_, _| Ok(()));
        timeout_seams::enable_timeouts::set(|_| Ok(()));
        timeout_seams::disable_timeout::set(|_, _| Ok(()));
        timeout_seams::disable_timeouts::set(|_| {});
        timeout_seams::get_timeout_start_time::set(|_| TEST_WAIT_START);
        timestamp_seams::get_current_timestamp::set(|| 0);

        ps_status_seams::set_ps_display_suffix::set(|_| {});
        ps_status_seams::set_ps_display_remove_suffix::set(|| {});
        elog_seams::ereport_msg::set(|level, msg, detail| {
            logged().lock().unwrap_or_else(|e| e.into_inner()).push((level, msg, detail));
            Ok(())
        });
        elog_seams::ereport_msg_detail_log::set(|level, msg, detail_log| {
            logged()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((level, msg, Some(format!("log-only: {detail_log}"))));
            Ok(())
        });
        elog_seams::ereport::set(|err| {
            assert!(
                err.detail().is_none() || !err.message().starts_with("process "),
                "lock-wait LOG carries a client-visible detail: {err:?}"
            );
            logged().lock().unwrap_or_else(|e| e.into_inner()).push((
                err.level(),
                err.message().to_string(),
                err.detail_log().or(err.detail()).map(|d| d.to_string()),
            ));
            Ok(())
        });
        lmgr_seams::describe_lock_tag::set(|tag| format!("{tag:?}"));
        procsignal_seams::send_thread_signal::set(|_, _| SIGNAL_ERRNO.load(SeqCst));

        shmem_seams::add_size::set(|a, b| Ok(a.checked_add(b).expect("size overflow")));
        shmem_seams::mul_size::set(|a, b| Ok(a.checked_mul(b).expect("size overflow")));
        shmem_seams::shmem_alloc::set(|size| {
            Ok(Box::leak(vec![0u8; size].into_boxed_slice()).as_mut_ptr())
        });

        lwlock::CreateLWLocks(false).unwrap();
        lmgr_proc::init_seams();
        init_seams();
        lmgr_proc::InitProcGlobal(&CFG);
        LockManagerShmemInit(CFG.max_prepared_xacts).unwrap();
    });
}

fn become_backend() {
    setup();
    thread_globals();
    if lmgr_proc::MyProc().is_none() {
        lmgr_proc::InitProcess(BackendType::Backend).unwrap();
        let procno = lmgr_proc::MyProc().unwrap();
        lmgr_proc::GetPGProcByNumber(procno)
            .databaseId
            .store(TESTDB, SeqCst);
        InitLockManagerAccess();
        let owner =
            resowner::ResourceOwnerCreate(types_resowner::ResourceOwner::NULL, "lock tests")
                .unwrap();
        resowner::SetCurrentResourceOwner(owner);
    }
}

fn rel_tag(relid: u32) -> LOCKTAG {
    LOCKTAG::relation(TESTDB, relid)
}

#[test]
fn conflict_table_matches_lock_h() {
    assert!(DoLockModesConflict(AccessShareLock, AccessExclusiveLock));
    assert!(!DoLockModesConflict(AccessShareLock, ExclusiveLock));
    assert!(DoLockModesConflict(RowExclusiveLock, ShareLock));
    assert!(DoLockModesConflict(ShareLock, RowExclusiveLock));
    assert!(!DoLockModesConflict(ShareLock, ShareLock));
    assert!(DoLockModesConflict(ExclusiveLock, ShareLock));
    assert_eq!(GetLockmodeName(1, AccessShareLock), "AccessShareLock");
    assert_eq!(GetLockmodeName(2, ExclusiveLock), "ExclusiveLock");
    assert_eq!(LOCK_CONFLICTS[AccessExclusiveLock as usize], 0b111111110);
}

#[test]
fn fastpath_acquire_and_release() {
    become_backend();
    let tag = rel_tag(6001);
    assert_eq!(
        LockAcquire(&tag, AccessShareLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    assert!(LockHeldByMe(&tag, AccessShareLock, false));
    assert_eq!(LockWaiterCount(&tag).unwrap(), 0);

    assert!(LockRelease(&tag, AccessShareLock, false).unwrap());
    assert!(!LockHeldByMe(&tag, AccessShareLock, false));
}

#[test]
fn reacquire_increments_local_count() {
    become_backend();
    let tag = rel_tag(6002);
    assert_eq!(
        LockAcquire(&tag, RowExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    assert_eq!(
        LockAcquire(&tag, RowExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_ALREADY_HELD
    );
    assert!(LockHeldByMe(&tag, AccessShareLock, true));
    assert!(!LockHeldByMe(&tag, AccessExclusiveLock, true));
    assert!(LockRelease(&tag, RowExclusiveLock, false).unwrap());
    assert!(LockHeldByMe(&tag, RowExclusiveLock, false));
    assert!(LockRelease(&tag, RowExclusiveLock, false).unwrap());
    assert!(!LockHeldByMe(&tag, RowExclusiveLock, false));
    assert!(!LockRelease(&tag, RowExclusiveLock, false).unwrap());
}

// D3652 P9: MarkLockClear right after an acquire writes through the grant pointer (main-table and fast-path grants);
// after the LOCALLOCK table changed (100 more entries inserted, so it may have rehashed and moved the entry; or a
// release) it takes the probe path. Either way the marked entry is the right one: the next acquire reads
// ALREADY_CLEAR on it and on nothing else.
#[test]
fn mark_lock_clear_through_the_grant_pointer_and_after_table_changes() {
    become_backend();
    let a = LOCKTAG::transaction(4301);
    assert_eq!(LockAcquire(&a, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_OK);
    MarkLockClear(&a, ExclusiveLock);
    assert_eq!(LockAcquire(&a, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_ALREADY_CLEAR);

    let r = rel_tag(6101);
    assert_eq!(LockAcquire(&r, AccessShareLock, false, false).unwrap(), LOCKACQUIRE_OK);
    MarkLockClear(&r, AccessShareLock);
    assert_eq!(LockAcquire(&r, AccessShareLock, false, false).unwrap(), LOCKACQUIRE_ALREADY_CLEAR);

    let b = LOCKTAG::transaction(4302);
    assert_eq!(LockAcquire(&b, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_OK);
    let others: Vec<LOCKTAG> = (0..100).map(|i| LOCKTAG::transaction(5000 + i)).collect();
    for t in &others {
        assert_eq!(LockAcquire(t, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_OK);
    }
    MarkLockClear(&b, ExclusiveLock);
    assert_eq!(LockAcquire(&b, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_ALREADY_CLEAR);

    let c = LOCKTAG::transaction(4303);
    assert_eq!(LockAcquire(&c, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_OK);
    assert!(LockRelease(&others[0], ExclusiveLock, false).unwrap());
    MarkLockClear(&c, ExclusiveLock);
    assert_eq!(LockAcquire(&c, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_ALREADY_CLEAR);
    for t in &others[1..] {
        assert_eq!(LockAcquire(t, ExclusiveLock, false, false).unwrap(), LOCKACQUIRE_ALREADY_HELD);
    }

    for t in [a, b, c] {
        assert!(LockRelease(&t, ExclusiveLock, false).unwrap());
        assert!(LockRelease(&t, ExclusiveLock, false).unwrap());
        assert!(!LockHeldByMe(&t, ExclusiveLock, false));
    }
    for _ in 0..2 {
        assert!(LockRelease(&r, AccessShareLock, false).unwrap());
    }
    for t in &others[1..] {
        assert!(LockRelease(t, ExclusiveLock, false).unwrap());
        assert!(LockRelease(t, ExclusiveLock, false).unwrap());
    }
}

#[test]
fn shared_table_transaction_lock() {
    become_backend();
    let tag = LOCKTAG::transaction(4242);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    assert_eq!(LockWaiterCount(&tag).unwrap(), 1);
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());
    assert_eq!(LockWaiterCount(&tag).unwrap(), 0);
}

#[test]
fn dontwait_conflict_returns_not_avail() {
    become_backend();
    let tag = LOCKTAG::transaction(4243);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    let t = std::thread::spawn(move || {
        become_backend();
        let r = LockAcquire(&tag, ShareLock, false, true).unwrap();
        assert_eq!(r, LOCKACQUIRE_NOT_AVAIL);
    });
    t.join().unwrap();
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());
}

#[test]
fn strong_lock_transfers_fastpath_to_shared_table() {
    become_backend();
    let tag = rel_tag(6003);
    assert_eq!(
        LockAcquire(&tag, AccessShareLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    assert_eq!(LockWaiterCount(&tag).unwrap(), 0);

    let t = std::thread::spawn(move || {
        become_backend();
        let r = LockAcquire(&tag, AccessExclusiveLock, false, true).unwrap();
        assert_eq!(r, LOCKACQUIRE_NOT_AVAIL);
    });
    t.join().unwrap();

    assert_eq!(LockWaiterCount(&tag).unwrap(), 1);
    assert!(LockRelease(&tag, AccessShareLock, false).unwrap());
    assert_eq!(LockWaiterCount(&tag).unwrap(), 0);
}

#[test]
fn blocked_acquire_wakes_on_release() {
    become_backend();
    let tag = LOCKTAG::transaction(4244);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );

    let t = std::thread::spawn(move || {
        become_backend();
        let r = LockAcquire(&tag, ShareLock, false, false).unwrap();
        assert_eq!(r, LOCKACQUIRE_OK);
        assert!(LockRelease(&tag, ShareLock, false).unwrap());
    });

    while LockWaiterCount(&tag).unwrap() < 2 {
        std::thread::yield_now();
    }
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());
    t.join().unwrap();
    assert_eq!(LockWaiterCount(&tag).unwrap(), 0);
}

#[test]
fn wakeup_clears_the_woken_procs_wait_start() {
    become_backend();
    let tag = LOCKTAG::transaction(4247);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    let waker = lmgr_proc::GetPGProcByNumber(lmgr_proc::MyProc().unwrap());
    const WAKER_STAMP: u64 = 4_242;
    waker.waitStart.write(WAKER_STAMP);

    let waiter_procno = std::sync::Arc::new(AtomicI32::new(-1));
    let publish = std::sync::Arc::clone(&waiter_procno);
    let t = std::thread::spawn(move || {
        become_backend();
        publish.store(lmgr_proc::MyProc().unwrap(), SeqCst);
        assert_eq!(
            LockAcquire(&tag, ShareLock, false, false).unwrap(),
            LOCKACQUIRE_OK
        );
        let mine = lmgr_proc::GetPGProcByNumber(lmgr_proc::MyProc().unwrap())
            .waitStart
            .read();
        assert!(LockRelease(&tag, ShareLock, false).unwrap());
        mine
    });

    // ProcSleep stamps waitStart after dropping the partition lock.
    loop {
        let procno = waiter_procno.load(SeqCst);
        if procno >= 0
            && LockWaiterCount(&tag).unwrap() >= 2
            && lmgr_proc::GetPGProcByNumber(procno).waitStart.read() == TEST_WAIT_START as u64
        {
            break;
        }
        std::thread::yield_now();
    }
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());
    assert_eq!(t.join().unwrap(), 0, "awakened proc keeps a stale waitStart");
    assert_eq!(waker.waitStart.read(), WAKER_STAMP, "waker's waitStart was cleared");
    waker.waitStart.write(0);
}

#[test]
fn lock_release_all_cleans_fastpath_and_shared() {
    let t = std::thread::spawn(|| {
        become_backend();
        let rel = rel_tag(6004);
        let xact = LOCKTAG::transaction(4245);
        assert_eq!(
            LockAcquire(&rel, AccessShareLock, false, false).unwrap(),
            LOCKACQUIRE_OK
        );
        assert_eq!(
            LockAcquire(&xact, ExclusiveLock, false, false).unwrap(),
            LOCKACQUIRE_OK
        );
        assert_eq!(
            LockAcquire(&xact, ExclusiveLock, true, false).unwrap(),
            LOCKACQUIRE_ALREADY_HELD
        );

        LockReleaseAll(1, false).unwrap();
        assert!(!LockHeldByMe(&rel, AccessShareLock, false));
        assert!(LockHeldByMe(&xact, ExclusiveLock, false));
        assert_eq!(LockWaiterCount(&xact).unwrap(), 1);

        LockReleaseAll(1, true).unwrap();
        assert!(!LockHeldByMe(&xact, ExclusiveLock, false));
        assert_eq!(LockWaiterCount(&xact).unwrap(), 0);
    });
    t.join().unwrap();
}

#[test]
fn early_grant_when_ahead_of_conflicting_waiter() {
    become_backend();
    let tag = rel_tag(6005);
    assert_eq!(
        LockAcquire(&tag, AccessShareLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );

    let t = std::thread::spawn(move || {
        become_backend();
        let r = LockAcquire(&tag, AccessExclusiveLock, false, false).unwrap();
        assert_eq!(r, LOCKACQUIRE_OK);
        assert!(LockRelease(&tag, AccessExclusiveLock, false).unwrap());
    });

    while LockWaiterCount(&tag).unwrap() < 2 {
        std::thread::yield_now();
    }

    assert_eq!(
        LockAcquire(&tag, RowExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    assert!(LockRelease(&tag, RowExclusiveLock, false).unwrap());
    assert!(LockRelease(&tag, AccessShareLock, false).unwrap());
    t.join().unwrap();
}

#[test]
fn shmem_size_positive() {
    setup();
    assert!(LockManagerShmemSize(CFG.max_prepared_xacts).unwrap() > 0);
}

#[test]
fn dlist_kernel_roundtrip() {
    use types_storage::ilist::dlist_head;
    use types_storage::lock::{PROCLOCK, PROCLOCKTAG};

    unsafe fn proclock() -> *mut PROCLOCK {
        Box::into_raw(Box::new(PROCLOCK {
            tag: PROCLOCKTAG::new(std::ptr::null_mut(), 0),
            groupLeader: 0,
            holdMask: 0,
            releaseMask: 0,
            lockLink: Default::default(),
            procLink: Default::default(),
        }))
    }

    unsafe {
        let mut head = dlist_head::new();
        assert!(crate::shared::dlist_is_empty(&head));
        let a = proclock();
        let b = proclock();
        let c = proclock();
        crate::shared::dlist_push_tail(&mut head, &raw mut (*a).lockLink);
        crate::shared::dlist_push_tail(&mut head, &raw mut (*b).lockLink);
        crate::shared::dlist_push_tail(&mut head, &raw mut (*c).lockLink);
        assert!(!crate::shared::dlist_is_empty(&head));

        let mut seen = Vec::new();
        let head_ptr: *mut dlist_head = &mut head;
        let mut cur = (*head_ptr).head.next;
        while let Some(node) = cur {
            cur = (*node.as_ptr()).next;
            seen.push(crate::shared::proclock_from_lock_link(node.as_ptr()));
        }
        assert_eq!(seen, vec![a, b, c]);

        crate::shared::dlist_delete(&mut head, &raw mut (*b).lockLink);
        crate::shared::dlist_delete(&mut head, &raw mut (*a).lockLink);
        crate::shared::dlist_delete(&mut head, &raw mut (*c).lockLink);
        assert!(crate::shared::dlist_is_empty(&head));

        drop(Box::from_raw(a));
        drop(Box::from_raw(b));
        drop(Box::from_raw(c));
    }
}

// Strong-lock release must publish the releaser's earlier sinval hasMessages
// store to any backend whose fast-path grant observes the release.
#[test]
fn strong_lock_release_publishes_prior_sinval_store() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;

    setup();

    const PART: u32 = 1013;
    const ROUNDS: usize = 500;

    static HAS_MESSAGES: AtomicBool = AtomicBool::new(false);
    static ROUND_START: AtomicUsize = AtomicUsize::new(0);
    static ROUND_DONE: AtomicUsize = AtomicUsize::new(0);

    let reader = std::thread::spawn(|| {
        for i in 1..=ROUNDS {
            while ROUND_START.load(SeqCst) < i {
                std::thread::yield_now();
            }
            while crate::fastpath::strong_lock_count(PART) != 0 {
                std::thread::yield_now();
            }
            assert!(
                HAS_MESSAGES.load(SeqCst),
                "round {i}: observed strong-lock release without the \
                 preceding sinval store — fastpath/sinval ordering broken"
            );
            ROUND_DONE.store(i, SeqCst);
        }
    });

    for i in 1..=ROUNDS {
        HAS_MESSAGES.store(false, SeqCst);
        crate::fastpath::increment_strong_lock_count_partition(PART);
        ROUND_START.store(i, SeqCst);
        HAS_MESSAGES.store(true, SeqCst);
        crate::fastpath::decrement_strong_lock_count_partition(PART);
        while ROUND_DONE.load(SeqCst) < i {
            std::thread::yield_now();
        }
    }
    reader.join().unwrap();
}

#[test]
fn dontwait_with_log_lock_failure_logs_holders_instead_of_panicking() {
    become_backend();
    let holder_pid = g::MyProcPid();
    let tag = LOCKTAG::transaction(4248);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    let t = std::thread::spawn(move || {
        become_backend();
        let my_pid = g::MyProcPid();
        let r = LockAcquireExtended(&tag, ShareLock, false, true, true, true).unwrap();
        assert_eq!(r, LOCKACQUIRE_NOT_AVAIL);
        my_pid
    });
    let waiter_pid = t.join().expect("dontWait + logLockFailure must not panic");
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());

    let expected_msg = format!(
        "process {waiter_pid} could not obtain ShareLock on {:?}",
        tag
    );
    let expected_detail = format!("Process holding the lock: {holder_pid}, Wait queue: .");
    let hit = {
        let logs = logged().lock().unwrap_or_else(|e| e.into_inner());
        logs.iter()
            .find(|(level, msg, _)| *level == types_error::LOG && *msg == expected_msg)
            .map(|(_, _, detail)| detail.clone())
            .unwrap_or_else(|| panic!("no LOG line {expected_msg:?} in {logs:?}"))
    };
    assert_eq!(hit.as_deref(), Some(expected_detail.as_str()));
}

#[test]
fn twophase_standby_recover_rejects_unknown_lock_method() {
    become_backend();
    for bad_method in [0u8, 3u8] {
        let mut tag = rel_tag(6100);
        tag.locktag_lockmethodid = bad_method;
        let mut rec = [0u8; crate::twophase::SIZEOF_TWOPHASE_LOCK_RECORD];
        rec[0..4].copy_from_slice(&tag.locktag_field1.to_ne_bytes());
        rec[4..8].copy_from_slice(&tag.locktag_field2.to_ne_bytes());
        rec[8..12].copy_from_slice(&tag.locktag_field3.to_ne_bytes());
        rec[12..14].copy_from_slice(&tag.locktag_field4.to_ne_bytes());
        rec[14] = tag.locktag_type;
        rec[15] = tag.locktag_lockmethodid;
        rec[16..20].copy_from_slice(&ShareLock.to_ne_bytes());
        let err = lock_twophase_standby_recover(4300, 0, &rec)
            .expect_err("unknown lock method must be an ERROR, not Ok(())");
        let text = format!("{err:?}");
        assert!(
            text.contains(&format!("unrecognized lock method: {bad_method}")),
            "unexpected error for lockmethodid {bad_method}: {text}"
        );
    }
}

fn owner_forgets_held_lock(tag: &LOCKTAG, lockmode: i32) {
    let owner = resowner::CurrentResourceOwner();
    resowner::ResourceOwnerRememberLock(
        owner,
        LOCALLOCKTAG {
            lock: rel_tag(6299),
            mode: AccessShareLock,
        },
    );
    let localtag = LOCALLOCKTAG {
        lock: *tag,
        mode: lockmode,
    };
    resowner::ResourceOwnerForgetLock(owner, localtag).unwrap();
}

const UNOWNED_LOCK_MSG: &str = "lock reference is not owned by resource owner lock tests";

fn assert_unowned_lock_error(err: Box<PgError>) {
    assert_eq!(err.level(), types_error::ERROR);
    assert_eq!(err.sqlstate(), types_error::ERRCODE_INTERNAL_ERROR);
    assert_eq!(err.message(), UNOWNED_LOCK_MSG);
}

#[test]
fn lock_release_reports_unowned_lock_reference_as_error() {
    become_backend();
    let tag = rel_tag(6200);
    assert_eq!(
        LockAcquire(&tag, AccessShareLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );
    owner_forgets_held_lock(&tag, AccessShareLock);
    let err = LockRelease(&tag, AccessShareLock, false)
        .expect_err("ResourceOwnerForgetLock must raise an ERROR, not Ok");
    assert_unowned_lock_error(err);
}

#[test]
fn lock_release_all_reports_unowned_lock_reference_as_error() {
    let t = std::thread::spawn(|| {
        become_backend();
        let tag = LOCKTAG::transaction(4260);
        assert_eq!(
            LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
            LOCKACQUIRE_OK
        );
        owner_forgets_held_lock(&tag, ExclusiveLock);
        let err = LockReleaseAll(1, false)
            .expect_err("ResourceOwnerForgetLock must raise an ERROR, not Ok");
        assert_unowned_lock_error(err);
    });
    t.join()
        .expect("LockReleaseAll with an unowned lock reference must not panic");
}

#[test]
fn lock_reassign_current_owner_reports_unowned_lock_reference_as_error() {
    let t = std::thread::spawn(|| {
        become_backend();
        let parent = resowner::CurrentResourceOwner();
        let child = resowner::ResourceOwnerCreate(parent, "lock tests").unwrap();
        resowner::SetCurrentResourceOwner(child);
        let tag = rel_tag(6202);
        assert_eq!(
            LockAcquire(&tag, AccessShareLock, false, false).unwrap(),
            LOCKACQUIRE_OK
        );
        owner_forgets_held_lock(&tag, AccessShareLock);
        let err = LockReassignCurrentOwner(None)
            .expect_err("ResourceOwnerForgetLock must raise an ERROR, not Ok");
        assert_unowned_lock_error(err);
        resowner::SetCurrentResourceOwner(parent);
    });
    t.join()
        .expect("LockReassignCurrentOwner with an unowned lock reference must not panic");
}

#[test]
fn autovacuum_cancel_failure_warning_carries_strerror_text() {
    become_backend();
    let autovac_procno = lmgr_proc::MyProc().unwrap();
    let autovac_pid = g::MyProcPid();
    let autovac = lmgr_proc::GetPGProcByNumber(autovac_procno);
    let off = (MAX_BACKENDS - 1) as usize;
    let saved_off = autovac.pgxactoff.swap(off as i32, SeqCst);
    lmgr_proc::ProcGlobal().statusFlags[off].store(types_storage::storage::PROC_IS_AUTOVACUUM, SeqCst);

    let tag = LOCKTAG::transaction(4249);
    assert_eq!(
        LockAcquire(&tag, ExclusiveLock, false, false).unwrap(),
        LOCKACQUIRE_OK
    );

    const EPERM: i32 = 1;
    let (tx, rx) = std::sync::mpsc::channel::<i32>();
    let t = std::thread::spawn(move || {
        become_backend();
        let me = lmgr_proc::MyProc().unwrap();
        AUTOVAC_BLOCKER.store(autovac_procno, SeqCst);
        SIGNAL_ERRNO.store(EPERM, SeqCst);
        BLOCKED_BY_AUTOVAC.store(me, SeqCst);
        DEADLOCK_ALERT_PENDING.store(me, SeqCst);
        tx.send(me).unwrap();
        let r = LockAcquire(&tag, ShareLock, false, false).unwrap();
        assert_eq!(r, LOCKACQUIRE_OK);
        assert!(LockRelease(&tag, ShareLock, false).unwrap());
    });
    let waiter = rx.recv().unwrap();
    let expected = format!(
        "could not send signal to process {autovac_pid}: Operation not permitted"
    );
    let mut seen = false;
    for _ in 0..20_000 {
        if logged()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(level, msg, _)| *level == types_error::WARNING && msg.starts_with("could not send signal to process"))
        {
            seen = true;
            break;
        }
        std::thread::yield_now();
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    assert!(LockRelease(&tag, ExclusiveLock, false).unwrap());
    t.join().unwrap();
    // proc.c:1555: the cancel line's detail is errdetail_log (server log only).
    assert!(logged()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(level, msg, detail)| *level == types_error::DEBUG1
            && msg.starts_with("sending cancel to blocking autovacuum PID")
            && detail.as_deref().is_some_and(|d| d.starts_with("log-only: Process "))));
    assert_ne!(DEADLOCK_ALERT_PENDING.load(SeqCst), waiter, "alert never consumed");
    lmgr_proc::ProcGlobal().statusFlags[off].store(0, SeqCst);
    autovac.pgxactoff.store(saved_off, SeqCst);
    BLOCKED_BY_AUTOVAC.store(-1, SeqCst);
    AUTOVAC_BLOCKER.store(-1, SeqCst);
    SIGNAL_ERRNO.store(0, SeqCst);
    assert!(seen, "no WARNING about the failed signal was logged");
    let warnings: Vec<String> = logged()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(level, msg, _)| *level == types_error::WARNING && msg.starts_with("could not send signal to process"))
        .map(|(_, msg, _)| msg.clone())
        .collect();
    assert!(
        warnings.iter().any(|m| *m == expected),
        "expected {expected:?}, got {warnings:?}"
    );
}

#[test]
fn shmem_init_registers_lock_hashes_with_c_shared_size() {
    use dynahash::{hash_get_shared_size, hash_select_dirsize};
    use types_hash::hsearch::{HASHCTL, HASH_DIRSIZE};
    setup();
    let shared_size = |max_size: i64| {
        let mut info = HASHCTL::new();
        info.dsize = hash_select_dirsize(max_size);
        info.max_dsize = info.dsize;
        hash_get_shared_size(&info, HASH_DIRSIZE)
    };
    let max_table_size = crate::shared::NLOCKENTS(CFG.max_prepared_xacts);
    let (_, found) = shmem::ShmemInitStruct("LOCK hash", shared_size(max_table_size)).unwrap();
    assert!(found, "\"LOCK hash\" is not registered in the ShmemIndex");
    let (_, found) =
        shmem::ShmemInitStruct("PROCLOCK hash", shared_size(max_table_size * 2)).unwrap();
    assert!(found, "\"PROCLOCK hash\" is not registered in the ShmemIndex");
}

#[test]
fn shmem_init_registers_fast_path_strong_relation_lock_data() {
    setup();
    let (_, found) = shmem::ShmemInitStruct("Fast Path Strong Relation Lock Data", 4100).unwrap();
    assert!(
        found,
        "\"Fast Path Strong Relation Lock Data\" (4100 bytes) is not registered in the ShmemIndex"
    );
}

#[test]
fn lock_wait_elapsed_clamps_backward_clock_step() {
    assert_eq!(waitqueue::lock_wait_elapsed(1_000_000, 999_000), (0, 0));
    assert_eq!(waitqueue::lock_wait_elapsed(5, 5), (0, 0));
    assert_eq!(waitqueue::lock_wait_elapsed(0, 1_234_567), (1234, 567));
    assert_eq!(waitqueue::lock_wait_elapsed(10, 1_010), (1, 0));
}

// lock.c:454-484: both tables start at half NLOCKENTS without HASH_FIXED_SIZE
// and grow through ShmemAllocNoError until the segment is exhausted.
#[test]
fn lock_tables_are_growable_from_half_size() {
    use types_hash::hsearch::{HASH_ENTER_NULL, HASH_REMOVE};
    setup();
    let tables = shared::shared();
    let max_table_size = shared::NLOCKENTS(CFG.max_prepared_xacts);
    let tags: Vec<LOCKTAG> = (0..max_table_size as u32 * 3 / 4)
        .map(|relid| LOCKTAG::relation(TESTDB + 1, 900_000 + relid))
        .collect();
    unsafe {
        assert!(!(*tables.lock_hash).isfixed);
        assert!(!(*tables.proclock_hash).isfixed);
        for tag in &tags {
            let key = tag as *const LOCKTAG as *const u8;
            let mut found = false;
            let p = dynahash::hash_search(tables.lock_hash, key, HASH_ENTER_NULL, Some(&mut found))
                .unwrap();
            assert!(!found);
            assert!(!p.is_null(), "growth past init_table_size must not be refused");
        }
        assert!(dynahash::hash_get_num_entries(tables.lock_hash) > max_table_size / 2);
        for tag in &tags {
            let key = tag as *const LOCKTAG as *const u8;
            dynahash::hash_search(tables.lock_hash, key, HASH_REMOVE, None).unwrap();
        }
    }
}
