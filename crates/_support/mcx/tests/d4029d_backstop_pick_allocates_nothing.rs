//! D4029 (d): the backstop's victim pick and no-victim view (the watchdog thread, the one that frees memory at the limit)
//! rank in place with a static pins table: no allocation.
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

/// The hosts' probe: backend 52 alone pins a 300 MiB part.
fn pins(f: &mut dyn FnMut(u32, u64)) {
    f(memledger::Acct::backend(52).0, 300 * MIB as u64);
}

fn statement(procno: u32, resident: usize, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<bool> {
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
        let canceled = governor::take_watchdog_cancel().is_some();
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        canceled
    })
}

#[test]
fn the_backstop_pick_and_its_view_allocate_nothing() {
    governor::set_part_pins_probe(pins);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(4));
    let others = [statement(51, 200 * MIB, tx.clone(), done.clone()), statement(53, 20 * MIB, tx.clone(), done.clone())];
    let pinner = statement(52, 100 * MIB, tx, done.clone());
    for _ in 0..3 {
        ready.recv().unwrap();
    }
    COUNTING.with(|c| c.set(true));
    let before = governor::no_victim_view();
    let v = governor::cancel_largest_victim(90, 32 * MIB);
    let after = governor::no_victim_view();
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0, "the pick and the views allocated");
    assert!(!before.pending_ack && before.stalled.is_none() && before.largest_other == 400 * MIB, "{before:?}");
    let v = v.expect("a victim");
    assert_eq!((v.backend, v.bytes), (Some((52, 7052)), 400 * MIB), "100 MiB resident + the 300 MiB part it alone pins");
    assert!(after.pending_ack, "its cancel inside the ack window");
    done.wait();
    assert!(pinner.join().unwrap());
    assert!(others.into_iter().all(|s| !s.join().unwrap()));
}
