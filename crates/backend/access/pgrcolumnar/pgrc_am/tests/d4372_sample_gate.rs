//! D4372: under `pgrust.memory_ladder = on` each window of ANALYZE's parallel pgrcolumnar sample passes the ladder's
//! gate before its threads start: under the line it runs at its dop; past it, after the climb, the wait and the hand-off,
//! it runs on the backend thread alone, and the next window past the line goes there without a wait. Never a refusal.
//! Ladder off: the dop, and the window's estimate is never computed.
#![cfg(not(any(loom, pgrust_sim)))]

use std::sync::atomic::{AtomicU64, Ordering};

const MIB: u64 = 1 << 20;
const LINE: u64 = 1000 * MIB;
const ASK: u64 = 64 * MIB;

static READ: AtomicU64 = AtomicU64::new(0);

fn read() -> Option<u64> {
    Some(READ.load(Ordering::SeqCst))
}
fn zero() -> u64 {
    0
}
fn none(_: u64) -> u64 {
    0
}
fn no_cancel(_: u64) -> bool {
    false
}
fn no() -> bool {
    false
}

static ACTS: mcx::ladder::Acts = mcx::ladder::Acts {
    read,
    reclaimable: zero,
    reclaim: none,
    cancel: no_cancel,
    cancel_in_flight: no,
    protected: zero,
    evict_protected: none,
};

fn ladder(mode: u8, read: u64) {
    mcx::ladder::install(&ACTS);
    READ.store(read, Ordering::SeqCst);
    mcx::ladder::publish(mode, LINE);
}

#[test]
fn d4372_a_sample_window_past_the_ladders_line_runs_alone_and_is_never_refused() {
    use pgrc_am::scan::sample_window_dop;
    mempolicy::consts::apply("bytes_wait_ms=40").expect("the wait budget knob");
    let mut serial = false;

    ladder(mcx::ladder::OFF, 2 * LINE);
    assert_eq!(sample_window_dop(8, &mut serial, || panic!("ladder off: no estimate")).expect("off"), 8, "ladder off: the dop");

    ladder(mcx::ladder::ON, 100 * MIB);
    assert_eq!(sample_window_dop(8, &mut serial, || ASK).expect("under"), 8, "under the line: the dop");
    assert!(!serial);

    ladder(mcx::ladder::ON, LINE + ASK);
    // the rungs run so far, R5's wait among them
    let runs = || mcx::ladder::counts().0.iter().map(|r| r.0).sum::<u64>();
    let before = runs();
    assert_eq!(sample_window_dop(8, &mut serial, || ASK).expect("past the line: never a refusal"), 1, "past the line: alone");
    assert!(serial, "the next window past the line goes alone");
    assert!(runs() > before, "the gate climbed and waited first");
    let before = runs();
    assert_eq!(sample_window_dop(8, &mut serial, || ASK).expect("past"), 1, "the next window past the line: alone");
    assert_eq!(runs(), before, "without a second climb or wait");

    ladder(mcx::ladder::ON, 100 * MIB);
    assert_eq!(sample_window_dop(8, &mut serial, || ASK).expect("under"), 8, "back under the line: the dop again");
    assert!(!serial);

    ladder(mcx::ladder::OFF, 0);
    mempolicy::consts::reset();
}
