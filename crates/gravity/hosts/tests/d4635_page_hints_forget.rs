//! D4635: the process-wide page hints kept an array per relation file forever (a `HashMap<RelFileLocator, Arc<[AtomicI32]>>`
//! charged to shared_buffers): nothing removed a dropped relation's or database's arrays, so the fast drift leg's DROP
//! DATABASEs grew the shared_buffers account +0.57 MiB a round. Now bufmgr's drop hooks let them go (`page_hints::forget`
//! at `DropRelationsAllBuffers`, `page_hints::forget_db` at `DropDatabaseBuffers`), the charge refit down before the arrays
//! free, so the forgetting backend's account ends where it started.

use std::alloc::{GlobalAlloc, Layout, System};

use gravity_hosts::ops::scan::page_hints::{self, PageHints};
use memledger::{Acct, Consumer};
use types_storage::{RelFileLocator, RelFileLocatorBackend};

/// Door 1, as the server's allocator wrapper counts it (init_small oom.rs).
struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static ALLOC: Door1 = Door1;

/// Blocks a relation of the test has: its array is 4 B a block (plus the table's growth headroom), far past a fold step.
const BLOCKS: u32 = 1 << 18;
const POOL: i32 = 1 << 20;
/// What the table's own map and a fold leave around the arrays: far below one array.
const SLACK: i64 = 64 << 10;

fn bytes(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
}

#[test]
fn a_dropped_relations_and_databases_hints_leave_the_table_and_its_charge() {
    std::thread::spawn(|| {
        memledger::set_floor(true);
        memledger::bind_backend(4635, 9635);
        let (me, sb) = (Acct::backend(4635), Acct::of(Consumer::SharedBuffers));
        let (m0, s0) = (bytes(me), bytes(sb));
        let (f0, _) = memledger::floored();
        let (db, other) = (46_350, 46_351);
        let rel = |db: u32, n: u32| RelFileLocator::new(1663, db, 900_000 + n);
        for n in 0..3 {
            drop(PageHints::of(rel(db, n), BLOCKS, POOL).expect("hints"));
        }
        drop(PageHints::of(rel(other, 0), BLOCKS, POOL).expect("hints"));
        let one = (BLOCKS as i64) * 4;
        assert!(bytes(sb) - s0 >= 4 * one, "four arrays on shared_buffers: {}", bytes(sb) - s0);
        assert!((bytes(me) - m0).abs() < SLACK, "none on the backend that made them: {}", bytes(me) - m0);

        // one relation file unlinked: its array leaves the table and the charge, freed here with nothing left on this account
        let before = bytes(sb);
        page_hints::forget(&[RelFileLocatorBackend { locator: rel(db, 0), backend: types_core::INVALID_PROC_NUMBER }]);
        assert!(before - bytes(sb) >= one, "the unlinked file's array left shared_buffers: {}", before - bytes(sb));
        assert!((bytes(me) - m0).abs() < SLACK, "and nets to 0 on the forgetting backend: {}", bytes(me) - m0);

        // the database dropped: its other two arrays go, the other database's stays
        page_hints::forget_db(db);
        let left = bytes(sb) - s0;
        assert!(left >= one && left < 2 * one, "only the other database's array is left: {left}");
        assert!((bytes(me) - m0).abs() < SLACK, "and the forgetting backend ends where it started: {}", bytes(me) - m0);
        page_hints::forget_db(other);
        assert!(bytes(sb) - s0 < SLACK, "every array of the test gone: {}", bytes(sb) - s0);
        let (f1, _) = memledger::floored();
        memledger::unbind_backend();
        memledger::set_floor(false);
        assert_eq!(f1 - f0, 0, "no fold of the forgets' frees floored the account (the charge refit down before the frees)");
    })
    .join()
    .unwrap();
}
