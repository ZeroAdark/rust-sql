//! D4092: a watchdog tier crossing (`pgrust.memory_watchdog_dump`) dumped the memory context trees of every live backend
//! at once (~70 on nestfast's m-stress, each stalled ~600 ms inside its kernel calls). It dumps the top holders by the
//! watchdog's own rank (`mempolicy::watchdog::DUMP_TOP`): `governor::try_top_holders` picks them, largest first, one per
//! backend, a pure read on the watchdog's thread that never waits or allocates.
#![cfg(feature = "std")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Counts the allocations a thread makes while its COUNTING is set.
struct Counting;

// SAFETY: a pass-through to System plus a thread-local counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const MIB: usize = 1 << 20;

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge (C8), whose drop gives
/// them back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

/// A running statement in backend `procno` (pid 7000 + procno) with `resident` bytes on its ledger account.
fn statement(procno: u32, resident: usize, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", resident);
        let pages = mapped(resident);
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
    })
}

#[test]
fn a_crossing_dumps_the_top_holders_by_rank_not_every_backend() {
    let sizes = [(61, 50), (62, 400), (63, 100), (64, 300), (65, 200)];
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(sizes.len() + 1));
    let threads: Vec<_> = sizes.iter().map(|&(p, m)| statement(p, m * MIB, tx.clone(), done.clone())).collect();
    drop(tx);
    for _ in 0..sizes.len() {
        ready.recv().unwrap();
    }
    let mut out = [governor::TopHolder::default(); 8];
    COUNTING.with(|c| c.set(true));
    let n = governor::try_top_holders(mempolicy::watchdog::DUMP_TOP, &mut out);
    COUNTING.with(|c| c.set(false));
    let allocs = ALLOCS.with(Cell::get);
    done.wait();
    threads.into_iter().for_each(|t| t.join().unwrap());
    let n = n.expect("nothing else holds the registry or the pins table");
    let got: Vec<(usize, i32, usize)> = out[..n].iter().map(|h| (h.slot, h.pid, h.bytes / MIB)).collect();
    assert_eq!(got, [(62, 7062, 400), (64, 7064, 300), (65, 7065, 200)], "the would-be victim and the next two by the watchdog's rank, not every one of the {} backends", sizes.len());
    assert_eq!(allocs, 0, "the pick allocates nothing (the watchdog's thread)");
}
