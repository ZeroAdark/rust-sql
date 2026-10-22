//! method_worker.c: the worker IO method. C's worker PROCESSES are
//! postmaster-child THREADS here (BackendType::IoWorker, pmchild-tracked,
//! PGPROC-owning); the worker slot stores the owner's ProcNumber and wakeups
//! go through its shared procLatch — the same latch C stores a raw pointer
//! to.

use std::sync::atomic::{AtomicPtr, Ordering};

use elog::ereport;
use init_small::globals as g;
use latch::{ResetLatch, SetLatch, WaitLatch};
use lwlock::{LWLockAcquire, LWLockRelease, LW_EXCLUSIVE};
use types_core::ProcNumber;
use types_error::{ErrorLocation, PgError, PgResult, DEBUG3, DEBUG4, ERROR};
use types_guc::PGC_SIGHUP;
use types_storage::aio::{PGAIO_HF_REFERENCES_LOCAL, PGAIO_SUBMIT_BATCH_SIZE};
use types_storage::latch::LatchHandle;
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET};

const MAX_IO_WORKERS_USIZE: usize = types_storage::storage::MAX_IO_WORKERS as usize;

use crate::handle::loc;
use crate::ioh;

const IO_WORKER_WAKEUP_FANOUT: usize = 2;
// C `io_worker_queue_size` (method_worker.c:100), a fixed 64 rounded to a
// power of two for the mask.
const IO_WORKER_QUEUE_SIZE: usize = 64;

// lwlocklist.h PG_LWLOCK(53, AioWorkerSubmissionQueue); pinned by test.
pub(crate) const AIO_WORKER_SUBMISSION_QUEUE_LOCK: usize = 53;

const PG_WAIT_ACTIVITY: u32 = 0x0500_0000;
const WAIT_EVENT_IO_WORKER_MAIN: u32 = PG_WAIT_ACTIVITY + 6;

const INVALID_PROC: ProcNumber = types_core::INVALID_PROC_NUMBER;

// method_worker.c:54-73: the two shmem blocks the worker method registers as
// "AioWorkerSubmissionQueue" and "AioWorkerControl" (pgaio_worker_shmem_init).
// Both are serialized by AioWorkerSubmissionQueueLock. C's flexible array
// members are fixed-size here (queue size is the constant 64; MAX_IO_WORKERS
// slots), so one struct each is the same size C requests.
struct PgAioWorkerSubmissionQueue {
    size: u32,
    mask: u32,
    head: u32,
    tail: u32,
    sqes: [u32; IO_WORKER_QUEUE_SIZE],
}

// C stores Latch*; the procno addresses the same shared procLatch. The
// padding keeps the slot at C's 16 bytes, so "AioWorkerControl" is 520.
#[repr(C, align(8))]
struct PgAioWorkerSlot {
    procno: ProcNumber,
    _latch_pad: u32,
    in_use: bool,
}

struct PgAioWorkerControl {
    idle_worker_mask: u64,
    workers: [PgAioWorkerSlot; MAX_IO_WORKERS_USIZE],
}

// C `io_worker_submission_queue` / `io_worker_control`: process-global
// pointers into shmem, set by pgaio_worker_shmem_init.
static SUBMISSION_QUEUE: AtomicPtr<PgAioWorkerSubmissionQueue> =
    AtomicPtr::new(std::ptr::null_mut());
static WORKER_CONTROL: AtomicPtr<PgAioWorkerControl> = AtomicPtr::new(std::ptr::null_mut());

/// SAFETY: caller holds AioWorkerSubmissionQueueLock (or runs the
/// single-threaded crash reset).
#[allow(clippy::mut_from_ref)]
unsafe fn queue() -> &'static mut PgAioWorkerSubmissionQueue {
    let p = SUBMISSION_QUEUE.load(Ordering::Acquire);
    debug_assert!(!p.is_null(), "pgaio_worker_shmem_init not called");
    &mut *p
}

/// SAFETY: as queue().
#[allow(clippy::mut_from_ref)]
unsafe fn control() -> &'static mut PgAioWorkerControl {
    let p = WORKER_CONTROL.load(Ordering::Acquire);
    debug_assert!(!p.is_null(), "pgaio_worker_shmem_init not called");
    &mut *p
}

fn queue_lock() -> &'static lwlock::LWLock {
    lwlock::main_lock(AIO_WORKER_SUBMISSION_QUEUE_LOCK)
}

fn pgaio_worker_queue_shmem_size() -> usize {
    std::mem::size_of::<PgAioWorkerSubmissionQueue>()
}

fn pgaio_worker_control_shmem_size() -> usize {
    std::mem::size_of::<PgAioWorkerControl>()
}

pub(crate) fn pgaio_worker_shmem_size() -> usize {
    pgaio_worker_queue_shmem_size() + pgaio_worker_control_shmem_size()
}

/// method_worker.c:133-158.
pub(crate) fn pgaio_worker_shmem_init(_first_time: bool) -> PgResult<()> {
    let (q_raw, found) =
        shmem::ShmemInitStruct("AioWorkerSubmissionQueue", pgaio_worker_queue_shmem_size())?;
    let q = q_raw.cast::<PgAioWorkerSubmissionQueue>();
    if !found {
        // SAFETY: fresh in-bounds allocation, single-threaded boot.
        unsafe {
            q.write(PgAioWorkerSubmissionQueue {
                size: IO_WORKER_QUEUE_SIZE as u32,
                mask: (IO_WORKER_QUEUE_SIZE - 1) as u32,
                head: 0,
                tail: 0,
                sqes: [0; IO_WORKER_QUEUE_SIZE],
            });
        }
    }
    SUBMISSION_QUEUE.store(q, Ordering::Release);

    let (c_raw, found) =
        shmem::ShmemInitStruct("AioWorkerControl", pgaio_worker_control_shmem_size())?;
    let c = c_raw.cast::<PgAioWorkerControl>();
    if !found {
        // SAFETY: as above.
        unsafe {
            c.write(PgAioWorkerControl {
                idle_worker_mask: 0,
                workers: [const { PgAioWorkerSlot { procno: INVALID_PROC, _latch_pad: 0, in_use: false } };
                    MAX_IO_WORKERS_USIZE],
            });
        }
    }
    WORKER_CONTROL.store(c, Ordering::Release);
    Ok(())
}

pub(crate) fn pgaio_worker_shmem_reset_after_crash() {
    if SUBMISSION_QUEUE.load(Ordering::Acquire).is_null() {
        return;
    }
    // Crash reset runs with all children dead: no lock needed.
    // SAFETY: single-threaded (postmaster crash cycle).
    let (q, c) = unsafe { (queue(), control()) };
    q.head = 0;
    q.tail = 0;
    c.idle_worker_mask = 0;
    for w in c.workers.iter_mut() {
        w.procno = INVALID_PROC;
        w.in_use = false;
    }
}

thread_local! {
    static MY_IO_WORKER_ID: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
}

pub fn pgaio_workers_enabled() -> bool {
    crate::io_method() == guc_tables::consts::IOMETHOD_WORKER
}

pub(crate) fn pgaio_worker_needs_synchronous_execution(index: u32) -> bool {
    !g::IsUnderPostmaster()
        || ioh(index).flags.load(Ordering::Relaxed) & PGAIO_HF_REFERENCES_LOCAL != 0
        || !crate::target::pgaio_io_can_reopen(index)
}

// SAFETY comment discipline: insert/consume/depth run under the queue lock.
fn queue_insert(q: &mut PgAioWorkerSubmissionQueue, index: u32) -> bool {
    let new_head = (q.head + 1) & q.mask;
    if new_head == q.tail {
        let report = ereport(DEBUG3);
        if !report.is_suppressed() {
            let _ = report.errhidestmt(true).errhidecontext(true)
                .errmsg_internal(format!("io queue is full, at {} elements", q.size))
                .finish(ErrorLocation::new("method_worker.c", 190, "pgaio_worker_submission_queue_insert"));
        }
        return false; // full
    }
    q.sqes[q.head as usize] = index;
    q.head = new_head;
    true
}

fn queue_consume(q: &mut PgAioWorkerSubmissionQueue) -> Option<u32> {
    if q.tail == q.head {
        return None;
    }
    let result = q.sqes[q.tail as usize];
    q.tail = (q.tail + 1) & q.mask;
    Some(result)
}

fn queue_depth(q: &PgAioWorkerSubmissionQueue) -> usize {
    let mut head = q.head as usize;
    let tail = q.tail as usize;
    if tail > head {
        head += q.size as usize;
    }
    head - tail
}

fn choose_idle(c: &mut PgAioWorkerControl) -> Option<usize> {
    if c.idle_worker_mask == 0 {
        return None;
    }
    let worker = c.idle_worker_mask.trailing_zeros() as usize;
    c.idle_worker_mask &= !(1u64 << worker);
    debug_assert!(c.workers[worker].in_use);
    Some(worker)
}

fn worker_debug_io(index: u32, message: std::fmt::Arguments<'_>, line: i32, function: &'static str) {
    let report = ereport(DEBUG4);
    if report.is_suppressed() {
        return;
    }
    let h = ioh(index);
    // SAFETY: the submitting backend or executing worker owns this queued IO.
    let d = unsafe { h.data() };
    let _ = report.errhidestmt(true).errhidecontext(true)
        .errmsg_internal(format!("io {:<10}|op {:<5}|target {:<4}|state {:<16}: {}",
            index, crate::pgaio_io_op_name(d.op), crate::pgaio_io_target_name(d.target),
            crate::pgaio_io_state_name(h.state.load(Ordering::Relaxed)), message))
        .finish(ErrorLocation::new("method_worker.c", line, function));
}

pub(crate) fn pgaio_worker_submit(staged: &[u32]) -> PgResult<()> {
    for &index in staged {
        crate::handle::pgaio_io_prepare_submit(index);
    }
    pgaio_worker_submit_internal(staged)
}

fn pgaio_worker_submit_internal(staged: &[u32]) -> PgResult<()> {
    debug_assert!(staged.len() <= PGAIO_SUBMIT_BATCH_SIZE);

    let mut synchronous_ios: [u32; PGAIO_SUBMIT_BATCH_SIZE] = [0; PGAIO_SUBMIT_BATCH_SIZE];
    let mut nsync = 0usize;
    let mut wakeup: Option<ProcNumber> = None;

    LWLockAcquire(queue_lock(), LW_EXCLUSIVE, g::MyProcNumber())?;
    // SAFETY: queue lock held.
    let (q, c) = unsafe { (queue(), control()) };
    for &index in staged {
        debug_assert!(!pgaio_worker_needs_synchronous_execution(index));
        if !queue_insert(q, index) {
            synchronous_ios[nsync] = index;
            nsync += 1;
            continue;
        }
        if wakeup.is_none() {
            let worker = choose_idle(c);
            if let Some(worker) = worker {
                wakeup = Some(c.workers[worker].procno);
            }
            worker_debug_io(index, format_args!("choosing worker {}", worker.map_or(-1, |w| w as i32)),
                274, "pgaio_worker_submit_internal");
        }
    }
    LWLockRelease(queue_lock())?;

    if let Some(procno) = wakeup {
        SetLatch(LatchHandle::proc(procno));
    }

    for &index in &synchronous_ios[..nsync] {
        crate::io::pgaio_io_perform_synchronously(index);
    }
    Ok(())
}

fn pgaio_worker_die(_code: i32, _arg: usize) {
    let id = MY_IO_WORKER_ID.get();
    debug_assert!(id >= 0);
    LWLockAcquire(queue_lock(), LW_EXCLUSIVE, g::MyProcNumber()).expect("pgaio_worker_die");
    // SAFETY: queue lock held.
    let c = unsafe { control() };
    debug_assert!(c.workers[id as usize].in_use);
    debug_assert!(c.workers[id as usize].procno == g::MyProcNumber());
    c.idle_worker_mask &= !(1u64 << id);
    c.workers[id as usize].in_use = false;
    c.workers[id as usize].procno = INVALID_PROC;
    LWLockRelease(queue_lock()).expect("pgaio_worker_die");
}


pub fn pgaio_worker_register() -> PgResult<()> {
    MY_IO_WORKER_ID.set(-1);

    LWLockAcquire(queue_lock(), LW_EXCLUSIVE, g::MyProcNumber())?;
    // SAFETY: queue lock held.
    let c = unsafe { control() };
    let mut my_id: i32 = -1;
    for (i, w) in c.workers.iter_mut().enumerate() {
        if !w.in_use {
            debug_assert!(w.procno == INVALID_PROC);
            w.in_use = true;
            my_id = i as i32;
            break;
        }
    }
    if my_id == -1 {
        LWLockRelease(queue_lock())?;
        ereport(ERROR)
            .errmsg_internal("couldn't find a free worker slot")
            .finish(loc("pgaio_worker_register"))?;
    }
    MY_IO_WORKER_ID.set(my_id);
    c.idle_worker_mask |= 1u64 << my_id;
    c.workers[my_id as usize].procno = g::MyProcNumber();
    LWLockRelease(queue_lock())?;

    ipc_seams::on_shmem_exit::call(pgaio_worker_die, 0);
    Ok(())
}

/// method_worker.c:365-383 pgaio_worker_error_callback: the errcontext line
/// an error raised while this worker executes `index` carries.
fn pgaio_worker_error_context(index: u32) -> String {
    let h = ioh(index);
    debug_assert!(h.owner_procno != g::MyProcNumber());
    debug_assert!(miscinit::GetMyBackendType() == types_core::BackendType::IoWorker);
    let owner_pid = lmgr_proc::GetPGProcByNumber(h.owner_procno).pid.load(Ordering::Relaxed);
    format!("I/O worker executing I/O on behalf of process {owner_pid}")
}

/// IoWorkerMain's sigsetjmp arm (method_worker.c:409-436) for an error
/// raised while `index` is being executed: emit the report (with the error
/// callback's CONTEXT line), release locks, fail the IO with -ENOENT (C's
/// error_errno — "there's not really a good errno we can report here"), and
/// exit 1 so the postmaster starts a fresh worker.
fn pgaio_worker_error_exit(index: u32, mut error: Box<PgError>) -> ! {
    error.add_context_line(pgaio_worker_error_context(index));
    g::HoldInterrupts();
    elog::emit_error_report_for(&error);
    let _ = lwlock::LWLockReleaseAll();
    g::StartCriticalSection();
    crate::handle::pgaio_io_process_completion(index, -libc::ENOENT);
    g::EndCriticalSection();
    ipc_seams::proc_exit::call(1, g::MyProcPid())
}

pub fn pgaio_worker_cycle() -> PgResult<()> {
    let mut latches: [ProcNumber; IO_WORKER_WAKEUP_FANOUT] = [INVALID_PROC; 2];
    let mut nlatches = 0usize;

    // C: the lwlock acquire is the barrier making the consumed handle's
    // fields visible.
    LWLockAcquire(queue_lock(), LW_EXCLUSIVE, g::MyProcNumber())?;
    let io_index;
    {
        // SAFETY: queue lock held.
        let (q, c) = unsafe { (queue(), control()) };
        let my_id = MY_IO_WORKER_ID.get() as usize;
        io_index = queue_consume(q);
        match io_index {
            None => {
                c.idle_worker_mask |= 1u64 << my_id;
            }
            Some(_) => {
                c.idle_worker_mask &= !(1u64 << my_id);
                let nwakeups = queue_depth(q).min(IO_WORKER_WAKEUP_FANOUT);
                for _ in 0..nwakeups {
                    match choose_idle(c) {
                        None => break,
                        Some(worker) => {
                            latches[nlatches] = c.workers[worker].procno;
                            nlatches += 1;
                        }
                    }
                }
            }
        }
    }
    LWLockRelease(queue_lock())?;

    for &procno in &latches[..nlatches] {
        SetLatch(LatchHandle::proc(procno));
    }

    match io_index {
        Some(index) => {
            worker_debug_io(index, format_args!("worker {} processing IO", MY_IO_WORKER_ID.get()),
                511, "IoWorkerMain");
            // C: interrupts held so the reopened fd cannot be closed before
            // execution consumes it.
            g::HoldInterrupts();

            // It's very unlikely, but possible, that reopen fails (memory
            // allocation, file permissions changing, ...). C's sigsetjmp arm
            // then fails the IO and exits the worker (method_worker.c:450-468).
            if let Err(e) = crate::target::pgaio_io_reopen(index) {
                pgaio_worker_error_exit(index, e);
            }

            // pgaio_io_perform_synchronously contains a critical section, so
            // it cannot fail with ERROR/FATAL.
            crate::io::pgaio_io_perform_synchronously(index);
            g::ResumeInterrupts();
        }
        None => {
            WaitLatch(
                g::MyLatch(),
                WL_LATCH_SET | WL_EXIT_ON_PM_DEATH,
                -1,
                WAIT_EVENT_IO_WORKER_MAIN,
            )?;
            ResetLatch(g::MyLatch().expect("io worker latch"));
        }
    }

    // method_worker.c:577-583: CHECK_FOR_INTERRUPTS (die/ProcDiePending,
    // barriers, ...) then the explicit SIGHUP reload. Shutdown requests are
    // the IoWorkerMain loop condition (proc_exit(0) past the loop).
    postgres_seams::check_for_interrupts::call()?;

    if interrupt::ConfigReloadPending() {
        interrupt::SetConfigReloadPending(false);
        guc_file_seams::process_config_file::call(PGC_SIGHUP)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_shmem_sizes_match_c() {
        // C on LP64: 16 + 4 * 64 and 8 + 16 * 32.
        assert_eq!(pgaio_worker_queue_shmem_size(), 272);
        assert_eq!(pgaio_worker_control_shmem_size(), 520);
        assert_eq!(pgaio_worker_shmem_size(), 792);
    }

    #[test]
    fn worker_debug_messages_match_c() {
        crate::tests::test_ctl();
        static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn hook(error: &PgError, output: &mut bool) {
            *output = false;
            assert!(error.hide_statement);
            assert!(error.hide_context);
            let location = error.location.as_ref().unwrap();
            assert_eq!(location.filename.as_deref(), Some("method_worker.c"));
            match SEEN.fetch_add(1, Ordering::Relaxed) {
                0 => {
                    assert_eq!(error.message, "io queue is full, at 64 elements");
                    assert_eq!(error.level, DEBUG3);
                    assert_eq!(location.lineno, 190);
                }
                1 => {
                    assert_eq!(error.message, "io 23        |op readv|target smgr|state SUBMITTED       : choosing worker -1");
                    assert_eq!(error.level, DEBUG4);
                    assert_eq!(location.lineno, 274);
                }
                2 => {
                    assert_eq!(error.message, "io 23        |op readv|target smgr|state SUBMITTED       : worker 2 processing IO");
                    assert_eq!(error.level, DEBUG4);
                    assert_eq!(location.lineno, 511);
                }
                _ => panic!("unexpected log"),
            }
        }
        let old_level = elog::config::log_min_messages();
        elog::config::set_log_min_messages(DEBUG4);
        let old_hook = elog::set_emit_log_hook(Some(hook));
        let mut q = PgAioWorkerSubmissionQueue {
            size: 64, mask: 63, head: 63, tail: 0, sqes: [0; 64],
        };
        assert!(!queue_insert(&mut q, 23));
        let h = ioh(23);
        // SAFETY: this handle is exclusively owned by this test.
        unsafe {
            h.data().op = types_storage::aio::PGAIO_OP_READV;
            h.data().target = types_storage::aio::PGAIO_TID_SMGR;
        }
        h.state.store(crate::PGAIO_HS_SUBMITTED, Ordering::Relaxed);
        worker_debug_io(23, format_args!("choosing worker -1"),
            274, "pgaio_worker_submit_internal");
        worker_debug_io(23, format_args!("worker 2 processing IO"),
            511, "IoWorkerMain");
        assert_eq!(SEEN.load(Ordering::Relaxed), 3);
        elog::set_emit_log_hook(old_hook);
        elog::config::set_log_min_messages(old_level);
        h.state.store(crate::PGAIO_HS_IDLE, Ordering::Relaxed);
    }
}
