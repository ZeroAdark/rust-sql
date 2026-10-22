//! D4023: a backstop victim answers its cancel at once, and its bytes leave resident over seconds. The next cancel used
//! to come 100 ms later (three spare 53200s in the decider's 15:04:55 burst); its bytes now stay pending for
//! `cancel_landing_ms` after the answer, which the backstop's hold (`mempolicy::watchdog::landing_holds`) reads.
#![cfg(feature = "std")]

use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;
use mempolicy::consts::{self, K};

const MIB: usize = 1 << 20;

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge (C8), whose drop gives
/// them back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
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
fn a_victims_bytes_stay_pending_after_it_answers() {
    consts::set(K::CancelLandingMs, 60_000);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(2));
    let big = statement(51, 400 * MIB, tx, done.clone());
    ready.recv().unwrap();
    assert_eq!(governor::decide_inputs().pending, 0);
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!(v.bytes, 400 * MIB);
    done.wait();
    assert!(big.join().unwrap(), "it answered and ended");
    assert_eq!(governor::decide_inputs().pending, 400 * MIB as u64, "its bytes are still landing");
    consts::set(K::CancelLandingMs, 1);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(2));
    let next = statement(52, 64 * MIB, tx, done.clone());
    ready.recv().unwrap();
    governor::cancel_largest_victim(90, 32 * MIB).expect("the next victim");
    done.wait();
    assert!(next.join().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(governor::decide_inputs().pending, 400 * MIB as u64, "the first still lands; the second (1 ms) has landed");
}
