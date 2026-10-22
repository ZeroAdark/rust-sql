//! D4812: the memory watchdog's cancel pick reads no residency on the watchdog's thread. D4059 read again, before a
//! cancel, the ranges of a statement whose region could have filled since the residency pass last read it; each read is
//! a mincore, which allocates a kernel page, and under host memory pressure that allocation waited in direct reclaim
//! (swap writeout, writeback throttling) for seconds with R6's cancel waiting behind it. Here the residency reader takes
//! 1.5 s, a stand-in for that reclaim, and the cancel must issue within one watchdog tick.
#![cfg(feature = "std")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant};

use mcx::governor;

const MIB: usize = 1 << 20;
/// The slow read: past ten ticks.
const SLOW_MS: u64 = 1500;

/// The reader: no page resident; or (`FILLED`) every page, after `SLOW_MS` while `SLOW` is set.
static FILLED: AtomicBool = AtomicBool::new(false);
static SLOW: AtomicBool = AtomicBool::new(false);

fn read(lo: usize, hi: usize, _scratch: &mut [u8]) -> Option<usize> {
    if !FILLED.load(Ordering::Relaxed) {
        return Some(0);
    }
    if SLOW.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(SLOW_MS));
    }
    Some(hi - lo)
}

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge, whose drop gives them
/// back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

/// A running statement on backend `procno` with `held` bytes on its ledger account; with `region`, those bytes are one
/// Gravity region on the residency registry, made fresh (none of its pages resident yet).
fn statement(procno: u32, held: usize, region: bool, ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<()> {
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
        let _ = governor::take_watchdog_cancel();
        if let Some(id) = id {
            memledger::resident::unregister(id);
        }
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
    })
}

#[test]
fn the_cancel_issues_within_a_tick_while_the_residency_read_is_slow() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    let filled = statement(95, 800 * MIB, true, tx.clone(), done.clone());
    let plain = statement(96, 300 * MIB, false, tx, done.clone());
    ready.recv().unwrap();
    ready.recv().unwrap();
    // the pass reads the region before its pages are written: 800 MiB not resident, so it could outrank the pick
    assert!(memledger::resident::pass(read, mempolicy::watchdog::RESIDENCY_READ_BYTES).is_some());
    assert_eq!(memledger::account_row(memledger::Acct::backend(95)).not_resident_bytes, 800 * MIB as i64);
    // then they are written, with no event of ours, and from here a residency read waits in "reclaim"
    FILLED.store(true, Ordering::Relaxed);
    SLOW.store(true, Ordering::Relaxed);
    let t = Instant::now();
    let v = governor::cancel_largest_victim(90, 32 * MIB);
    let took = t.elapsed();
    SLOW.store(false, Ordering::Relaxed);
    // the pick is the residency thread's last read's, counted stale, and the filled statement goes to that thread, whose
    // next step reads it
    let p = governor::pick_counts();
    let hinted = memledger::resident::hint_counts().0;
    let read_again = memledger::resident::refresh_hinted();
    let nr_after = memledger::account_row(memledger::Acct::backend(95)).not_resident_bytes;
    done.wait();
    filled.join().unwrap();
    plain.join().unwrap();
    assert!(v.is_some(), "a victim");
    assert_eq!(v.and_then(|v| v.backend), Some((96, 7096)), "picked from the last read: the 300 MiB statement: {v:?}");
    assert_eq!((p.picks, p.stale_picks), (1, 1), "the cancel is counted stale: {p:?}");
    assert_eq!(hinted, 1, "the filled statement was handed to the residency thread");
    assert_eq!(read_again, 1, "the residency thread read it again");
    assert_eq!(nr_after, 0, "and found it resident");
    assert!(
        took < Duration::from_millis(mempolicy::watchdog::GOVERNOR_CADENCE_MS as u64),
        "the cancel took {} ms with a {SLOW_MS} ms residency read (the pick waited on the read's mincore: D4812)",
        took.as_millis()
    );
}
