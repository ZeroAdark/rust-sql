//! D4812: the cancel pick is not stale against itself. A pick whose own region filled since the residency thread's last
//! read ranks under its resident bytes until the next read, but read again it only ranks higher and stays the pick. So
//! the cancel is not counted stale, and neither an episode's sample nor the pick hands that statement to the residency
//! thread. A stress's hog, whose regions fill all the time, made every cancel count stale and every sample hint it.
#![cfg(feature = "std")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};

use mcx::governor;

const MIB: usize = 1 << 20;
const PROCNO: u32 = 97;
const HELD: usize = 800 * MIB;
/// The region's first byte, where `statement` places it.
const LO: usize = 0x7000_0000_0000 + ((PROCNO as usize) << 32);

/// The test's residency reader: the pages below this address are resident.
static RESIDENT_BELOW: AtomicUsize = AtomicUsize::new(0);

fn read(lo: usize, hi: usize, _scratch: &mut [u8]) -> Option<usize> {
    Some(RESIDENT_BELOW.load(Ordering::Relaxed).saturating_sub(lo).min(hi - lo))
}

/// `n` bytes of door-2 pages committed on the calling thread's running statement: an owned charge, whose drop gives them
/// back.
fn mapped(n: usize) -> memledger::Charge {
    let (owner, release) = memledger::charge::context();
    memledger::Charge::open(owner, release, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n as u64)
}

/// A running statement on backend `PROCNO` with `HELD` bytes on its ledger account, one Gravity region on the residency
/// registry at `LO`, made fresh (none of its pages resident yet).
fn statement(ready: mpsc::Sender<()>, done: Arc<Barrier>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pid = 7000 + PROCNO as i32;
        governor::register(PROCNO as i32, pid);
        memledger::bind_backend(PROCNO, pid as u32);
        governor::statement_start(0, true);
        memledger::statement_begin();
        let charge = governor::Charge::new("held", HELD);
        let pages = mapped(HELD);
        let id = memledger::resident::register(LO, HELD, HELD, memledger::Acct::backend(PROCNO), true, true);
        memledger::fold();
        ready.send(()).unwrap();
        done.wait();
        let _ = governor::take_watchdog_cancel();
        memledger::resident::unregister(id);
        drop(pages);
        drop(charge);
        let _ = memledger::statement_end();
        governor::statement_end();
        memledger::unbind_backend();
    })
}

#[test]
fn a_pick_whose_own_region_filled_is_not_counted_stale_or_handed_over() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(2));
    let hog = statement(tx, done.clone());
    ready.recv().unwrap();
    // the pass reads the region half written: 400 MiB not resident
    RESIDENT_BELOW.store(LO + HELD / 2, Ordering::Relaxed);
    let passed = memledger::resident::pass(read, mempolicy::watchdog::RESIDENCY_READ_BYTES).is_some();
    let nr = memledger::account_row(memledger::Acct::backend(PROCNO)).not_resident_bytes;
    // then the rest is written, with no event of ours: read again, the statement would only rank higher
    RESIDENT_BELOW.store(usize::MAX, Ordering::Relaxed);
    let sampled = governor::try_hint_stale(32 * MIB, true);
    let v = governor::cancel_largest_victim(90, 32 * MIB);
    let p = governor::pick_counts();
    let hinted = memledger::resident::hint_counts().0;
    let read_again = memledger::resident::refresh_hinted();
    done.wait();
    hog.join().unwrap();
    assert!(passed, "the residency pass ran");
    assert_eq!(nr, (HELD / 2) as i64, "the last read found half the region resident");
    assert_eq!(sampled, Ok(0), "the episode's sample handed the pick itself to the residency thread");
    assert_eq!(v.as_ref().map(|v| (v.backend, v.bytes)), Some((Some((97, 7097)), HELD / 2)), "the pick, ranked by the last read: {v:?}");
    assert_eq!((p.picks, p.stale_picks), (1, 0), "a pick is not stale against itself: {p:?}");
    assert_eq!((hinted, read_again), (0, 0), "nothing went to the residency thread");
}
