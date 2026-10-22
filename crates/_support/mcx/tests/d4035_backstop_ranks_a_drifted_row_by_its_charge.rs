//! D4035: a backend's ledger row goes below its statement's start when another account's bytes are freed on it (memtest:
//! rows down to -1864 MiB under the stress mix). The backstop ranked by that row clamped at 0, so the statement holding
//! the most could rank under cancel_min and hide. A row below its start now ranks by the statement's charge.
#![cfg(feature = "std")]

use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;

const MIB: usize = 1 << 20;

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge (C8), whose drop gives
/// them back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

/// A statement on backend `procno` charging and committing `held`; `freed_elsewhere` more is freed on its account (bytes
/// another account committed) before it reports ready.
fn statement(procno: u32, held: usize, freed_elsewhere: usize, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", held);
        let pages = mapped(held);
        // another account's allocator bytes freed on this thread (door 1): the row drifts, no owner's charge moves
        memledger::door_heap(-(freed_elsewhere as isize));
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        let canceled = governor::take_watchdog_cancel().is_some();
        memledger::door_heap(freed_elsewhere as isize);
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        canceled
    })
}

#[test]
fn a_statement_whose_row_drifted_below_its_start_ranks_by_its_charge() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    // 400 MiB held, 600 MiB of another account's freed here: its row reads -200 MiB
    let drifted = statement(81, 400 * MIB, 600 * MIB, tx.clone(), done.clone());
    let honest = statement(82, 100 * MIB, 0, tx, done.clone());
    for _ in 0..2 {
        ready.recv().unwrap();
    }
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!((v.backend, v.bytes), (Some((81, 7081)), 400 * MIB), "its 400 MiB charge, not its row clamped at 0: {v:?}");
    done.wait();
    assert!(drifted.join().unwrap());
    assert!(!honest.join().unwrap());
}
