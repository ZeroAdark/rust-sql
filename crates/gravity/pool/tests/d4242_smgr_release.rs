//! D4242: a pool thread runs no transaction, so PostgreSQL's AtEOXact_SMgr never empties its smgr relation table: every
//! relation its claims read by locator keeps an entry, with its md segment files open, after DROP TABLE / DATABASE. The
//! table must let go of its unpinned entries at a claim's start once a backend has released its own files for another
//! backend's change (smgrreleaserellocator, smgrreleaseall), or once it holds more than its cap; otherwise it stays.
//! On trunk d9ced47a062 the entry outlives the release (targblock 7 read back).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use gravity_parallel::{BlockRange, ClaimSize, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig};
use types_core::primitive::{InvalidBlockNumber, INVALID_PROC_NUMBER};
use types_storage::{RelFileLocator, RelFileLocatorBackend};

fn rel(n: u32) -> RelFileLocatorBackend {
    RelFileLocatorBackend { locator: RelFileLocator { spcOid: 1663, dbOid: 4_242, relNumber: 900_000 + n }, backend: INVALID_PROC_NUMBER }
}

struct OneThread {
    pool: RuntimePool,
    thread: Mutex<Option<ThreadId>>,
}

impl OneThread {
    /// One claim on the pool's only thread: `job` runs there, after the claim's start.
    fn claim(&self, job: impl Fn() + Sync) {
        let claims = BlockRange::new(1, ClaimSize::Fixed(1));
        let thread = &self.thread;
        let f: Box<Job<'_>> = Box::new(move |_ordinal, _range| {
            let me = std::thread::current().id();
            assert_eq!(*thread.lock().unwrap().get_or_insert(me), me, "every claim runs on the one pool thread");
            job();
            Ok(())
        });
        self.pool.run(1, &claims, &f).expect("section");
    }

    fn targblock(&self, key: RelFileLocatorBackend) -> u32 {
        let seen = AtomicU32::new(0);
        self.claim(|| seen.store(smgr::smgrgettargblock(key), Ordering::Relaxed));
        seen.load(Ordering::Relaxed)
    }
}

#[test]
fn a_pool_threads_smgr_entries_go_after_a_release_or_past_the_cap() {
    let rt = Runtime::new(RuntimeConfig::new(1));
    let workers = runtime::WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool thread");
    let one = OneThread { pool: RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS), thread: Mutex::new(None) };

    let a = rel(1);
    one.claim(|| {
        smgr::smgropen(a.locator, a.backend).expect("smgropen");
        smgr::smgrsettargblock(a, 7);
    });
    assert_eq!(one.targblock(a), 7, "with nothing released, a later claim keeps the thread's entry (no reopen per claim)");

    // what a leader's smgr inval for a dropped relation runs (inval local.rs -> smgrreleaserellocator)
    smgr::smgrreleaserellocator(a).expect("release");
    assert_eq!(one.targblock(a), InvalidBlockNumber, "after a release, the pool thread's next claim starts without the entry");

    let b = rel(2);
    one.claim(|| {
        for n in 10..1_110 {
            let k = rel(n);
            smgr::smgropen(k.locator, k.backend).expect("smgropen");
        }
        smgr::smgropen(b.locator, b.backend).expect("smgropen");
        smgr::smgrsettargblock(b, 9);
    });
    assert_eq!(one.targblock(b), InvalidBlockNumber, "past the cap, the next claim starts with an empty table");
    workers.shutdown();
}
