//! D4029: past the line with no statement at cancel_min, the backstop saw nothing to do while a canceled statement that
//! never answered grew to the kernel's kill (memtest d3982-wm1g: 4.1 s of silence). Its view now names that statement
//! (how long unanswered, its rank at the cancel and now) and the largest other, for `mempolicy::watchdog::no_victim`.
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

/// A statement on backend `procno` holding `resident`, then `grow` more once told; it never answers its cancel.
fn statement(procno: u32, resident: usize, grow: usize, ready: mpsc::Sender<()>, go: Arc<Barrier>, done: Arc<Barrier>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", resident);
        let mut pages = mapped(resident);
        memledger::fold();
        ready.send(()).unwrap();
        go.wait();
        pages.commit(grow as u64);
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
fn the_view_names_the_canceled_statement_that_never_answered_and_grew() {
    let (tx, ready) = mpsc::channel();
    let (go, done) = (Arc::new(Barrier::new(3)), Arc::new(Barrier::new(3)));
    let hog = statement(71, 400 * MIB, 60 * MIB, tx.clone(), go.clone(), done.clone());
    let small = statement(72, 20 * MIB, 0, tx, go.clone(), done.clone());
    for _ in 0..2 {
        ready.recv().unwrap();
    }
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("the hog");
    assert_eq!((v.backend, v.bytes), (Some((71, 7071)), 400 * MIB));
    assert_eq!(governor::cancel_largest_victim(90, 32 * MIB), None, "nothing else at cancel_min");
    let young = governor::no_victim_view();
    assert!(young.pending_ack && young.stalled.is_none(), "inside its ack window: {young:?}");
    std::thread::sleep(std::time::Duration::from_millis(mempolicy::watchdog::CANCEL_ACK_MS + 50));
    go.wait();
    for _ in 0..2 {
        ready.recv().unwrap();
    }
    let old = governor::no_victim_view();
    let s = old.stalled.expect("the unanswered hog");
    assert!(!old.pending_ack && s.age_ms >= mempolicy::watchdog::CANCEL_ACK_MS, "{old:?}");
    assert_eq!((s.backend, s.ranked, s.now), (Some((71, 7071)), 400 * MIB, 460 * MIB), "it grew since its cancel");
    assert_eq!(old.largest_other, 20 * MIB);
    assert_eq!(mempolicy::watchdog::no_victim(old.pending_ack, Some((s.now > s.ranked, None)), false), mempolicy::watchdog::NoVictim::Terminate);
    done.wait();
    hog.join().unwrap();
    small.join().unwrap();
}
