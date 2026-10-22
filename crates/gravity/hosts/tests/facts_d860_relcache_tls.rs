//! D860: the fixture's relcache (`common::RELCACHE`, a thread-local per test thread) was built in ONE process-wide memory
//! context (`relcache_mcx`, leaked once). A test thread's thread-locals are dropped when the thread exits, after the test
//! returned and released `serial()`, so the exiting thread freed its relation entries into the shared context while the next
//! test's thread allocated and freed in it. A memory context is not thread-safe: the two threads raced on its freelists and
//! (debug builds) its freed-chunk set, `hashbrown` saw a corrupt table (`attempt to subtract with overflow`, raw/mod.rs) and
//! the abort came from the thread-local destructor (`thread local panicked on drop`). d263_scratch hit it once under
//! parallel crate-test load (SHEPHERD, e9ed3ad0c88); standalone the teardown rarely overlaps the next test.
//!
//! The test runs that overlap on purpose: a spawned thread opens the fixture's heap and index (its `RELCACHE` fills) and exits
//! while this thread keeps opening relations through the same seam (allocations and frees in the fixture's relcache memory).
//! Before the fix it aborts the test binary; after it each thread's relcache lives in that thread's own context.
use gravity_hosts_rig as common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::*;
use mcx::MemoryContext;
use types_storage::lock::AccessShareLock;

const ROUNDS: usize = 400;

#[test]
fn a_test_thread_s_relcache_teardown_does_not_race_another_thread_s_relation_opens() {
    let _g = serial();
    install();
    set_unique_index(true);
    reset_fixture();
    for _ in 0..ROUNDS {
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let t = std::thread::spawn(move || {
            let ctx = MemoryContext::new("d860 opener");
            for _ in 0..8 {
                let h = relation_seams::relation_open::call(ctx.mcx(), HEAP_OID, AccessShareLock).unwrap();
                let i = relation_seams::relation_open::call(ctx.mcx(), IDX_OID, AccessShareLock).unwrap();
                drop((h, i));
                rebuild_relcache(); // the entries go, the next open rebuilds them: frees + allocations in the relcache memory
            }
            let _h = relation_seams::relation_open::call(ctx.mcx(), HEAP_OID, AccessShareLock).unwrap();
            d.store(true, Ordering::Release);
            // the thread exits here: its RELCACHE thread-local drops its entries (the frees D860 raced)
        });
        let ctx = MemoryContext::new("d860 main");
        // until the opener has finished, its thread-local teardown included (a panicked opener finishes too)
        while !t.is_finished() {
            let h = relation_seams::relation_open::call(ctx.mcx(), HEAP2_OID, AccessShareLock).unwrap();
            drop(h);
            let h = relation_seams::relation_open::call(ctx.mcx(), HEAP_OID, AccessShareLock).unwrap();
            drop(h);
            rebuild_relcache();
        }
        t.join().expect("the opener thread");
        assert!(done.load(Ordering::Acquire), "the opener did not finish its opens");
    }
    quiesced();
}
