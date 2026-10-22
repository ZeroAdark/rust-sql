//! D4059: the backstop ranks a running statement by its ledger bytes less the bytes the residency pass found not resident
//! at its last read of the statement's ranges. A region whose pages were written after that read (no event of ours: the
//! pass reads an unchanged, partly resident range again only every span / RESIDENCY_READ_BYTES passes, about 3 s for
//! 4 GiB) hid that much of the statement, and the cancel went to a smaller one. D4812: the backstop reads no range itself
//! (a mincore on its thread waited in direct reclaim for seconds). The samples of a ladder episode hand a statement that
//! could outrank the pick to the residency thread, which reads its ranges again before the cancel.
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

#[test]
fn the_backstop_reads_a_region_that_filled_since_the_last_pass_before_it_cancels() {
    let (tx, ready) = mpsc::channel();
    let done = Arc::new(Barrier::new(3));
    let filled = statement(93, 800 * MIB, true, tx.clone(), done.clone());
    let plain = statement(94, 300 * MIB, false, tx, done.clone());
    ready.recv().unwrap();
    ready.recv().unwrap();
    // the pass reads the region before its pages are written: 800 MiB not resident on its statement's account
    RESIDENT.store(0, Ordering::Relaxed);
    assert!(memledger::resident::pass(read, mempolicy::watchdog::RESIDENCY_READ_BYTES).is_some());
    assert_eq!(memledger::account_row(memledger::Acct::backend(93)).not_resident_bytes, 800 * MIB as i64);
    // then they are written, with no event of ours; an episode's sample hands the statement to the residency thread,
    // whose next step reads it again
    RESIDENT.store(usize::MAX, Ordering::Relaxed);
    assert_eq!(governor::try_hint_stale(32 * MIB, true), Ok(1), "the sample hands the filled statement over");
    assert_eq!(memledger::resident::refresh_hinted(), 1, "the residency thread reads it");
    let v = governor::cancel_largest_victim(90, 32 * MIB).expect("a victim");
    assert_eq!(v.backend, Some((93, 7093)), "the statement whose region filled (800 MiB), not the 300 MiB one: {v:?}");
    assert_eq!(v.bytes, 800 * MIB, "ranked by what is resident now");
    assert_eq!(governor::pick_counts().stale_picks, 0, "the pick found nothing stale");
    done.wait();
    assert!(filled.join().unwrap(), "the filled statement is canceled");
    assert!(!plain.join().unwrap(), "the smaller one runs on");
}
