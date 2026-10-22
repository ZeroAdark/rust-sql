//! LEDGER2 step 4 (born red), D4374's shape: R6 cancelled a bystander (40 MB resident, 222 MB charged) while the statement
//! holding the memory ranked under it. Today's rank reads the backend's row, and a row below its statement's start falls
//! back to the charge: the bystander ranks by 222 MiB it does not hold. The real holder's memory sits in its owner tree
//! (an operator's arena, its contexts) more than its row shows. With `pgrust.memory_ledger_v2` on, R6 ranks a statement
//! by max(its owner tree's bytes, its row with its parts) and picks the holder; off, the pick is today's.
#![cfg(feature = "std")]

use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;

const MIB: usize = 1 << 20;

/// `n` bytes on the calling thread's running statement's owner tree only (a door-1 owned charge on its own account: the
/// owner's cell, never its row).
fn tree(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Sort, memledger::charge::Door::Heap, n as u64)
}

/// `n` bytes of door-2 pages on the calling thread's running statement: on its owner tree and its row.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

struct Shape {
    charged: usize,
    in_tree: usize,
    on_row: usize,
    drift: usize,
}

/// A statement on backend `procno`: `charged` bytes on its governor charge, `in_tree` on its owner tree alone, `on_row`
/// door-2 pages (tree and row), and `drift` bytes of another account freed on its row.
fn statement(procno: u32, s: Shape, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", s.charged);
        let t = tree(s.in_tree);
        let pages = mapped(s.on_row);
        memledger::door_heap(-(s.drift as isize));
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        let canceled = governor::take_watchdog_cancel().is_some();
        memledger::door_heap(s.drift as isize);
        drop(pages);
        drop(t);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        canceled
    })
}

/// R6's pick over a bystander (procno `by`) and the holder (`holder`): the victim's backend.
fn pick(by: u32, holder: u32, v2: bool) -> Option<usize> {
    mcx::ledger2::set_on(v2);
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    // 222 MiB charged, 40 MiB held (its tree), its row 600 MiB under its start: the charge fallback ranks it at 222 MiB
    let b = statement(by, Shape { charged: 222 * MIB, in_tree: 40 * MIB, on_row: 0, drift: 600 * MIB }, tx.clone(), done.clone());
    // 50 MiB charged, 100 MiB on its row, 300 MiB more in its tree (an operator arena's chunks the row does not show)
    let h = statement(holder, Shape { charged: 50 * MIB, in_tree: 300 * MIB, on_row: 100 * MIB, drift: 0 }, tx, done.clone());
    for _ in 0..2 {
        ready.recv().unwrap();
    }
    let v = governor::cancel_largest_victim(90, 32 * MIB);
    done.wait();
    let (cb, ch) = (b.join().unwrap(), h.join().unwrap());
    mcx::ledger2::set_on(false);
    let who = v.and_then(|v| v.backend).map(|(slot, _)| slot);
    assert_eq!((cb, ch), (who == Some(by as usize), who == Some(holder as usize)), "the cancel landed on the pick");
    who
}

#[test]
fn ledger2_r6_with_the_flag_on_picks_the_holder_not_the_charged_bystander() {
    assert_eq!(pick(4074, 4075, false), Some(4074), "off: today's pick, the bystander by its charge");
    let (runs0, _, _) = mcx::ledger2::counts()[mcx::ledger2::Decision::Cancel as usize];
    assert_eq!(pick(4076, 4077, true), Some(4077), "on: the holder, by its owner tree's 400 MiB");
    let (runs1, _, _) = mcx::ledger2::counts()[mcx::ledger2::Decision::Cancel as usize];
    assert!(runs1 > runs0, "the pick is a shadow decision too");
    let (scans, ns, max) = mcx::ledger2::scan_cost();
    assert!(ns >= max, "the operator-cell scans are timed: {scans} scans, {ns} ns, max {max} ns");
}
