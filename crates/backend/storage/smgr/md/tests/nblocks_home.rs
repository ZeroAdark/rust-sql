//! md's relation size cache keeps its map's nodes in its own home (the `nblocks_cache` consumer's account): a session's
//! inserts do not stay on that session's account, and another session's purge of them (a DROP DATABASE's `purge_db`) does
//! not take them off its own. Before this the nodes stayed on the inserting session (on UNBOUND after its unbind) and the
//! purge's frees left the dropping session's account that far below zero. Its own binary: the global allocator is door 1.

use std::alloc::{GlobalAlloc, Layout, System};

use memledger::{account_row, bind_backend, door_heap, fold, set_home, unbind_backend, Acct};
use types_core::primitive::ForkNumber;
use types_storage::RelFileLocator;

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// Relations noted: the map's nodes for them are several hundred KiB.
const RELS: u32 = 20_000;
/// What a spawn / join moves between accounts: far below the nodes' bytes.
const SLACK: i64 = 64 << 10;
const DB: u32 = 81;

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

/// The holder's account, found by its name (none before the holder existed).
fn holder() -> Option<Acct> {
    (1..memledger::BACKEND_SLOT_BASE).map(Acct).find(|a| a.name() == "nblocks_cache")
}

#[test]
fn a_dropped_databases_sizes_leave_the_cache_not_the_dropping_session() {
    set_home(true);
    let u0 = account(Acct::UNBOUND);
    let h0 = holder().map(account);
    // a session of database 81 reads the size of each of its relations, then ends
    let inserted = std::thread::spawn(|| {
        bind_backend(4060, 9060);
        let a = Acct::backend(4060);
        let s0 = account(a);
        for rel in 1..=RELS {
            let l = RelFileLocator::new(1663, DB, rel);
            let t = md::nblocks_cache::lookup_or_begin_walk(l, ForkNumber::MAIN_FORKNUM).expect_err("a miss");
            md::nblocks_cache::note_walked(l, ForkNumber::MAIN_FORKNUM, rel, t);
        }
        let d = account(a) - s0;
        unbind_backend();
        d
    })
    .join()
    .unwrap();
    let held = holder().map(account);
    // another session drops database 81
    let (dropper, unbound) = std::thread::spawn(|| {
        bind_backend(4061, 9061);
        let a = Acct::backend(4061);
        let d0 = account(a);
        md::nblocks_cache::purge_db(DB);
        let d = account(a) - d0;
        unbind_backend();
        (d, account(Acct::UNBOUND))
    })
    .join()
    .unwrap();
    let l = RelFileLocator::new(1663, DB, 1);
    assert_eq!(md::nblocks_cache::lookup(l, ForkNumber::MAIN_FORKNUM), None, "purged");
    assert!(dropper.abs() < SLACK, "the dropping session's account moved {dropper} B");
    assert!(inserted.abs() < SLACK, "the inserting session's account kept {inserted} B of the cache's nodes");
    assert!((unbound - u0).abs() < SLACK, "the inserting session's bytes on UNBOUND: {} B", unbound - u0);
    let (h0, held, back) = (h0.expect("the cache's holder account"), held.unwrap(), holder().map(account).unwrap());
    assert!(held - h0 > 4 * SLACK, "the cache's account held its nodes: {} B", held - h0);
    assert!((back - h0).abs() < SLACK, "the cache's account is back at its start after the purge: {} B", back - h0);
}
