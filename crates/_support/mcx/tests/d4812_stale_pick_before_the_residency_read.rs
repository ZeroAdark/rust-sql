//! D4812, the degraded road of D4059's pick: the backstop reads no range itself (a mincore on its thread waited in direct
//! reclaim for seconds), so when a region filled since the residency thread's last read and no sample handed its
//! statement over first (or that thread has not read it yet), the pick ranks it by that last read. The cancel goes to the
//! smaller statement and is counted stale, the filled one is handed to the residency thread, and once that thread read
//! it the next cancel goes to it.
#![cfg(feature = "std")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;

const MIB: usize = 1 << 20;

/// The test's residency reader: every range has this many bytes resident, at most its span.
static RESIDENT: AtomicUsize = AtomicUsize::new(0);

fn read(lo: usize, hi: usize, _scratch: &mut [u8]) -> Option<usize> {
    Some(RESIDENT.load(Ordering::Relaxed).min(hi - lo))
}

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge, whose drop gives them
/// back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

/// A running statement on backend `procno` with `held` bytes on its ledger account; with `region`, those bytes are one
/// Gravity region on the residency registry, made fresh (none of its pages resident yet). It returns whether it was
/// canceled.
fn statement(procno: u32, held: usize, region: bool, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", held);
        let pages = mapped(held);
        let lo = 0x7000_0000_0000 + ((procno as usize) << 32);
        let id = region.then(|| memledger::resident::register(lo, held, held, memledger::Acct::backend(procno), true, true));
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        let canceled = governor::take_watchdog_cancel().is_some();
        if let Some(id) = id {
            memledger::resident::unregister(id);
        }
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        canceled
    })
}

/// `statement` without a region, which ends as soon as the watchdog cancels it (its 53200 raised), then says so.
fn smaller(procno: u32, held: usize, ready: mpsc::Sender<()>, ended: mpsc::Sender<()>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let pid = 7000 + procno as i32;
        governor::register(procno as i32, pid);
        memledger::bind_backend(procno, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", held);
        let pages = mapped(held);
        memledger::fold();
        ready.send(()).unwrap();
        let t = std::time::Instant::now();
        let mut canceled = false;
        while !canceled && t.elapsed() < std::time::Duration::from_secs(10) {
            canceled = governor::take_watchdog_cancel().is_some();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
        ended.send(()).unwrap();
        canceled
    })
}

#[test]
fn before_the_residency_threads_read_the_pick_is_the_last_reads_and_counted_stale_and_the_next_reads_it() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(2));
    let filled = statement(93, 800 * MIB, true, tx.clone(), done.clone());
    let (ended_tx, ended) = mpsc::channel();
    let plain = smaller(94, 300 * MIB, tx, ended_tx);
    ready.recv().unwrap();
    ready.recv().unwrap();
    // the pass reads the region before its pages are written: 800 MiB not resident on its statement's account
    RESIDENT.store(0, Ordering::Relaxed);
    assert!(memledger::resident::pass(read, mempolicy::watchdog::RESIDENCY_READ_BYTES).is_some());
    assert_eq!(memledger::account_row(memledger::Acct::backend(93)).not_resident_bytes, 800 * MIB as i64);
    // then they are written, with no event of ours: the pick ranks by the last read, and the filled statement goes to
    // the residency thread
    RESIDENT.store(usize::MAX, Ordering::Relaxed);
    let first = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!(first.backend, Some((94, 7094)), "the last read's pick: {first:?}");
    assert_eq!(governor::pick_counts().stale_picks, 1, "counted stale");
    assert!(plain.join().unwrap(), "the smaller one is canceled");
    ended.recv().unwrap();
    // the residency thread's next step reads it again
    assert_eq!(memledger::resident::refresh_hinted(), 1);
    assert_eq!(memledger::account_row(memledger::Acct::backend(93)).not_resident_bytes, 0);
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!(v.backend, Some((93, 7093)), "the statement whose region filled (800 MiB): {v:?}");
    assert_eq!(v.bytes, 800 * MIB, "ranked by what is resident now");
    done.wait();
    assert!(filled.join().unwrap(), "the filled statement is canceled");
}
