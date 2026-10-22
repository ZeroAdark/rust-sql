//! Tier 0's asynchronous promotion (C03 / DR-21): a code-cache miss that tier 0 takes hands its module's compile to this
//! pool, which files it in the process code cache beside the interpreted first run, so the program's next execution
//! promotes from a hit instead of compiling inline. Nothing waits on the pool but a compile of the same key (cache.rs
//! `claim`). A full queue, a stopped pool or a thread the OS refuses drops the job: that promotion compiles inline, as every
//! promotion did before the pool. A failed or panicking compile is counted and logged and leaves the key unfiled.
//! The threads live as long as the server process; `stop` drops what is queued and ends them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_ir::{Func, Program};
use gravity_program::cost::runtime::{COMPILE_POOL_THREADS, COMPILE_QUEUE_MAX};
use pgsync::{Condvar, Mutex};

/// `PGRUST_GRAVITY_TIER0_ASYNC=off`: no pool (every promotion compiles inline: the arm before it), read at each miss tier 0
/// takes (a compile's rate, not a statement's).
pub const ASYNC_ENV: &str = "PGRUST_GRAVITY_TIER0_ASYNC";

struct Job {
    program: Arc<Program>,
    entries: Arc<[Func]>,
    /// D3811's fault injection (`crate::TIER0_HOLD_ENV`, read at server start): ms the compile holds its key first.
    hold_ms: u64,
    /// The module's code key (`cache::key_of`), taken once by the submitter (D3886: the pool's compile hashed the IR again).
    key: crate::cache::CodeKey,
}

struct Queue {
    jobs: VecDeque<Job>,
    threads: usize,
    running: usize,
    stopped: bool,
}

impl Queue {
    /// Queue `job` unless the pool is stopped or `cap` jobs wait already.
    fn push(&mut self, job: Job, cap: usize) -> bool {
        if self.stopped || self.jobs.len() >= cap {
            return false;
        }
        self.jobs.push_back(job);
        true
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: VecDeque::new(), threads: 0, running: 0, stopped: false });
static WAKE: Condvar = Condvar::new();
static IDLE: Condvar = Condvar::new();

/// Jobs queued, modules the pool filed, jobs dropped (queue full, stopped, no thread), compiles that failed or panicked.
pub static QUEUED: AtomicU64 = AtomicU64::new(0);
pub static FILED: AtomicU64 = AtomicU64::new(0);
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
pub static FAILED: AtomicU64 = AtomicU64::new(0);

fn on() -> bool {
    !matches!(gravity_driver::env::text(ASYNC_ENV).as_deref().map(str::trim), Some("off"))
}

/// Hand `program`'s module (every entry: one compile serves them all, D230) to the pool; false = not queued. `hold_ms`:
/// the compile holds the module's key that long first (D3811's fault injection, 0 = none).
pub(crate) fn submit(program: Arc<Program>, entries: Arc<[Func]>, hold_ms: u64, key: crate::cache::CodeKey) -> bool {
    if !on() {
        return false;
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if !q.push(Job { program, entries, hold_ms, key }, COMPILE_QUEUE_MAX) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if q.threads < COMPILE_POOL_THREADS.min(q.running + q.jobs.len()) {
        match pgsync::thread::Builder::new().name("gravity-tier0-compile".into()).spawn(pagegive::pinned(worker)) {
            Ok(_) => q.threads += 1,
            Err(_) if q.threads == 0 => {
                q.jobs.pop_back();
                DROPPED.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            Err(_) => {}
        }
    }
    QUEUED.fetch_add(1, Ordering::Relaxed);
    WAKE.notify_one();
    true
}

fn worker() {
    loop {
        let job = {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.stopped {
                    q.threads -= 1;
                    IDLE.notify_all();
                    return;
                }
                if let Some(j) = q.jobs.pop_front() {
                    q.running += 1;
                    break j;
                }
                q = WAKE.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let exit = run(job);
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.running -= 1;
        if exit.is_some() {
            q.threads -= 1;
        }
        if q.jobs.is_empty() && q.running == 0 || exit.is_some() {
            IDLE.notify_all();
        }
        // D2962: a designed exit ends this thread unchanged (its slot is free: the next submit spawns another)
        if let Some(payload) = exit {
            drop(q);
            std::panic::resume_unwind(payload);
        }
    }
}

/// One job: the module through the cache (a key another compile files first is taken from it, cache.rs `claim`). No error
/// leaves the pool: a failure is the next miss's to compile again. A designed exit's payload (D2962) is returned for the
/// worker to rethrow once its bookkeeping is done.
fn run(job: Job) -> Option<Box<dyn std::any::Any + Send>> {
    // D4762: the pool compiles for the code cache: the job's claim, compile, filing, the codes it drops and the job itself
    // (a program its statement let go drops here) are the program caches' (`cache::home`), never this thread's (UNBOUND),
    // so every context this thread drops parks its keeper as theirs, and every keeper it takes is theirs
    let _home = crate::cache::home().enter();
    let exit = match gravity_abi::status::unwind::catch(|| crate::pool_compiled_all(&job.program, &job.entries, job.hold_ms, job.key)) {
        Ok(Ok((_, o))) if o.lookup == crate::Lookup::Miss => {
            FILED.fetch_add(1, Ordering::Relaxed);
            None
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            FAILED.fetch_add(1, Ordering::Relaxed);
            eprintln!("gravity backend: tier 0's asynchronous compile failed, its next execution compiles inline: {}", e.message());
            None
        }
        Err(p) if types_error::is_exit_payload(&*p) => Some(p),
        Err(p) => {
            FAILED.fetch_add(1, Ordering::Relaxed);
            let msg = p.downcast_ref::<String>().map(String::as_str).or_else(|| p.downcast_ref::<&str>().copied()).unwrap_or("(non-string payload)");
            eprintln!("gravity backend: tier 0's asynchronous compile panicked, its next execution compiles inline: {msg}");
            None
        }
    };
    drop(job);
    exit
}

/// Every queued job compiled (or dropped) and none running (tests, measurements).
pub fn quiesce() {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    while !(q.jobs.is_empty() && q.running == 0) && q.threads > 0 {
        q = IDLE.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

/// End the pool for this process: queued jobs are dropped (counted), a running compile finishes and files, the threads
/// exit; later misses promote inline.
pub fn stop() {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    q.stopped = true;
    DROPPED.fetch_add(q.jobs.len() as u64, Ordering::Relaxed);
    q.jobs.clear();
    WAKE.notify_all();
    while q.threads > 0 {
        q = IDLE.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

/// The pool's threads now (tests).
pub fn threads() -> usize {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).threads
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let f = pb.declare_func("e", &[gravity_ir::Type::Ptr], &[gravity_ir::Type::I64]);
        let mut b = pb.define(f);
        let k = b.iconst(gravity_ir::Type::I64, 0);
        b.ret(&[k]);
        drop(b);
        let program = pb.finish();
        let key = crate::cache::key_of(&program);
        Job { program: Arc::new(program), entries: vec![f].into(), hold_ms: 0, key }
    }

    #[test]
    fn the_queue_takes_jobs_up_to_its_cap_and_none_once_stopped() {
        let mut q = Queue { jobs: VecDeque::new(), threads: 0, running: 0, stopped: false };
        for i in 0..3 {
            assert!(q.push(job(), 3), "job {i}");
        }
        assert!(!q.push(job(), 3), "full: dropped");
        assert_eq!(q.jobs.len(), 3);
        q.jobs.clear();
        q.stopped = true;
        assert!(!q.push(job(), 3), "stopped: dropped");
        assert!(q.jobs.is_empty());
    }
}
