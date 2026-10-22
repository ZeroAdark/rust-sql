//! D3999: a running scan's in-use parts sit on the part cache's account, so the backstop's ranking missed the statement
//! that pins most. It counts the parts a statement pins ALONE (the hosts' probe): canceling one of several users of a
//! shared part frees none of it.
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

/// The hosts' probe over a cache where backends 41 and 42 share a 900 MiB part and 43 pins a 300 MiB one alone.
fn alone(f: &mut dyn FnMut(u32, u64)) {
    f(memledger::Acct::backend(43).0, 300 * MIB as u64);
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
fn the_backstop_counts_the_parts_a_statement_pins_alone() {
    governor::set_part_pins_probe(alone);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(4));
    // by the split the sharers rank 200 + 450 MiB each; what a cancel frees: 200, 200, and 100 + 300 MiB
    let sharers = [statement(41, 200 * MIB, tx.clone(), done.clone()), statement(42, 200 * MIB, tx.clone(), done.clone())];
    let lone = statement(43, 100 * MIB, tx, done.clone());
    for _ in 0..3 {
        ready.recv().unwrap();
    }
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!((v.backend, v.bytes), (Some((43, 7043)), 400 * MIB), "100 MiB resident + the 300 MiB part it alone pins: {v:?}");
    done.wait();
    assert!(lone.join().unwrap());
    assert!(sharers.into_iter().all(|s| !s.join().unwrap()), "the sharers run on");
}
