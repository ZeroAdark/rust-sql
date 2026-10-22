//! D3992: the backstop ranked by charge (pid 1160: 4208 MB charged, 85 MB resident); it ranks by resident bytes.
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

/// A statement charged `charged` bytes with `resident` on its ledger account; it returns the cancel it saw.
fn statement(procno: u32, pid: i32, charged: usize, resident: usize, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<Option<String>> {
    std::thread::spawn(move || {
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let grant = governor::Charge::new("Gravity grant", charged);
        let pages = mapped(resident);
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        let detail = governor::take_watchdog_cancel().and_then(|e| e.detail().map(str::to_string));
        drop(pages);
        drop(grant);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        detail
    })
}

#[test]
fn the_backstop_cancels_the_resident_statement_not_the_reserved_one() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    // both past cancel_min: ordered by charge the 4 GiB reservation goes first, by resident the other one does
    let reserved = statement(31, 7031, 4108 * MIB, 100 * MIB, tx.clone(), done.clone());
    let resident = statement(32, 7032, 700 * MIB, 600 * MIB, tx, done.clone());
    ready.recv().unwrap();
    ready.recv().unwrap();
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!(v.backend, Some((32, 7032)), "the statement holding resident memory, not the 4 GiB reservation: {v:?}");
    assert_eq!((v.bytes, v.charged), (600 * MIB, 700 * MIB), "ranked by resident bytes, the charge beside");
    done.wait();
    assert_eq!(reserved.join().unwrap(), None, "the reserved statement runs on");
    let detail = resident.join().unwrap().expect("the resident statement's 53200");
    assert!(detail.contains("ranked by resident memory (614400 kB resident, 716800 kB charged)"), "{detail}");
}
