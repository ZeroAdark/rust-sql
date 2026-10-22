//! D4398: the memory ladder's thread never waits on a cache's lock. Its R2a and R2b reach each cache through hooks that
//! take the cache's own lock, so a thread holding one (a load, a trim, a block's asker waiting on the ladder, D4397)
//! stalled the ladder thread: no sample, no cancel, for as long as it held it. Inside the ladder's pass
//! (`memledger::ladder_pass::LadderPass`, memruntime's sample) a cache whose lock another thread holds is left out of the
//! pass, and the walk goes on to the next cache; outside a pass (the reclaimer, the backstop, the gates) the lock is
//! waited for as before. Born red: trunk has no pass (the test does not build there), and its R2a waited on the lock.
//! One test: the budget's registry is process-wide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;
use memledger::ladder_pass::{lock_or_skip, LadderPass};
use pgsync::Mutex;

const MIB: u64 = 1 << 20;

/// The catalog L2's bytes under its own lock (as its shards are), and the code cache's.
static L2: Mutex<u64> = Mutex::new(0);
static CODE: AtomicU64 = AtomicU64::new(0);

fn l2() -> u64 {
    lock_or_skip(&L2).map_or(0, |g| *g)
}
fn l2_trim(to: u64) -> u64 {
    let Some(mut g) = lock_or_skip(&L2) else { return 0 };
    let freed = g.saturating_sub(to);
    *g -= freed;
    freed
}
fn none_in_use() -> u64 {
    0
}
fn code() -> u64 {
    CODE.load(Ordering::Relaxed)
}
fn code_in_use() -> u64 {
    50 * MIB
}
fn code_trim(to: u64) -> u64 {
    let (cur, to) = (code(), to.max(code_in_use()));
    if to >= cur {
        return 0;
    }
    CODE.store(to, Ordering::Relaxed);
    cur - to
}

/// `f` on a thread of its own inside a ladder pass, and how long it took.
fn in_pass<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> (T, Duration) {
    std::thread::spawn(move || {
        let _pass = LadderPass::enter();
        let at = Instant::now();
        let out = f();
        (out, at.elapsed())
    })
    .join()
    .unwrap()
}

#[test]
fn the_ladders_pass_leaves_out_a_cache_whose_lock_another_thread_holds_and_frees_the_next() {
    *L2.lock().unwrap() = 300 * MIB;
    CODE.store(200 * MIB, Ordering::Relaxed);
    assert!(budget::register(Consumer::CatalogL2, Hooks { current: l2, floor: l2, cap: l2, reclaim: None }));
    assert!(budget::register(Consumer::ProgramCaches, Hooks { current: code, floor: code, cap: code, reclaim: None }));
    budget::register_ladder_reclaim(Consumer::CatalogL2, none_in_use, l2_trim);
    budget::register_ladder_reclaim(Consumer::ProgramCaches, code_in_use, code_trim);
    budget::note_b(4096 * MIB);

    // this thread holds the L2's lock while the ladder's pass runs: R2a's count leaves it out, its reclaim skips it and
    // frees the code cache next, and the pass returns at once
    let held = L2.lock().unwrap();
    let ((elastic, freed), took) = in_pass(|| (reclaim::ladder_elastic(), reclaim::ladder_reclaim(350 * MIB)));
    drop(held);
    assert_eq!(elastic, 150 * MIB, "the held L2 is left out of R2a's count: the code cache's 150 MiB above its in use");
    assert_eq!(freed, 150 * MIB, "R2a skips the held L2 and frees the code cache");
    assert!(took < Duration::from_millis(500), "the pass never waited on the lock: {took:?}");
    assert_eq!((*L2.lock().unwrap(), code()), (300 * MIB, code_in_use()), "the held cache is untouched");

    // the next pass with the lock free takes the L2
    let (freed, _) = in_pass(|| reclaim::ladder_reclaim(350 * MIB));
    assert_eq!((freed, *L2.lock().unwrap()), (300 * MIB, 0));

    // outside a pass (the reclaimer, the backstop, a gate's own climb) the lock is waited for, as before
    *L2.lock().unwrap() = 100 * MIB;
    let (took_it, taken) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let held = L2.lock().unwrap();
        took_it.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
    });
    taken.recv().unwrap();
    let at = Instant::now();
    assert_eq!(reclaim::ladder_reclaim(100 * MIB), 100 * MIB, "waited for, then freed");
    assert!(at.elapsed() >= Duration::from_millis(150), "outside a pass the lock was waited for: {:?}", at.elapsed());
    holder.join().unwrap();
}
