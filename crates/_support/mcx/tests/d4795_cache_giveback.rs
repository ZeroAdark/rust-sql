//! D4795 (2a): a part-cache fault charged to a running statement's governor count (`charge_cache`, the holder's record) is
//! taken back when the part is freed on any thread while that statement runs, and never from another statement: a free
//! after the statement ended, or after its backend started the next one, takes nothing; a free racing the next start
//! never lands on it.
#![cfg(feature = "std")]

use mcx::governor::{self, CacheCharge};

const N: usize = 3 << 20;

/// `f` on a fresh thread bound to governor slot `procno`, inside a watched statement.
fn on(procno: i32, f: impl FnOnce(&std::sync::Arc<governor::StmtMem>) + Send + 'static) {
    std::thread::spawn(move || {
        governor::register(procno, 70_000 + procno);
        governor::statement_start(0, true);
        let l = governor::current().expect("watch turns the ledger on");
        f(&l);
        governor::statement_end();
    })
    .join()
    .unwrap();
}

fn free_elsewhere(n: usize, rec: CacheCharge) -> CacheCharge {
    std::thread::spawn(move || {
        let mut r = rec;
        governor::release_cache(n, &mut r);
        r
    })
    .join()
    .unwrap()
}

#[test]
fn a_part_freed_on_another_thread_mid_statement_leaves_the_statements_count() {
    on(4001, |l| {
        let before = l.bytes();
        let mut rec = CacheCharge::default();
        governor::charge_cache(N, "pgrc part cache", &mut rec);
        assert_eq!(l.bytes() - before, N as isize, "the fault is on the statement's count");
        let held = l.held();
        let rec = free_elsewhere(N, rec);
        assert_eq!(l.bytes(), before, "the free on another thread takes exactly the part's bytes back");
        assert_eq!(held - l.held(), N as u64, "held() with them");
        assert_eq!(rec, CacheCharge::default(), "the holder's record is spent");
    });
}

#[test]
fn a_part_freed_after_its_statement_ended_changes_nothing() {
    on(4002, |l| {
        let mut rec = CacheCharge::default();
        governor::charge_cache(N, "pgrc part cache", &mut rec);
        governor::statement_end();
        let ended = l.bytes();
        free_elsewhere(N, rec);
        assert_eq!(l.bytes(), ended, "a free after the end takes nothing from the ended statement");
        governor::statement_start(0, true);
    });
}

#[test]
fn a_free_of_the_last_statements_part_leaves_the_next_statement_alone() {
    on(4003, |l| {
        let mut old = CacheCharge::default();
        governor::charge_cache(N, "pgrc part cache", &mut old);
        governor::statement_end();
        governor::statement_start(0, true);
        let start = l.bytes();
        let mut new = CacheCharge::default();
        governor::charge_cache(N / 3, "pgrc part cache", &mut new);
        let charged = l.bytes();
        assert_eq!(charged - start, (N / 3) as isize);
        free_elsewhere(N, old);
        assert_eq!(l.bytes(), charged, "the slot's epoch moved: the earlier statement's free is not this one's");
        free_elsewhere(N / 3, new);
        assert_eq!(l.bytes(), start);
    });
}

/// Frees racing the backend's next statement start never land on that statement (each statement holds `PIN` until its
/// end, so a free that leaked in would take its count under `PIN`), and once the frees stop the share is exactly its own
/// again: odd sizes net in the share's KiB with nothing left over. Each statement waits for a pair the worker ran after
/// its own charge before it ends (D4832: on a loaded host the worker can be scheduled only after every round has run, so
/// no free would race any start), so the frees race every boundary by construction.
#[test]
fn frees_racing_the_next_statement_start_never_land_on_it() {
    const PIN: usize = 5 << 20;
    const ROUNDS: usize = 400;
    on(4004, |l| {
        use std::sync::atomic::Ordering::{Acquire, Release};
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (l2, stop2, ran2) = (l.clone(), stop.clone(), ran.clone());
        let mut w = Some(std::thread::spawn(move || {
            let _in = governor::enter(Some(l2));
            let mut pairs = 0u64;
            while !stop2.load(Acquire) {
                let n = 1000 + (pairs as usize % 7) * 4099;
                let mut rec = CacheCharge::default();
                governor::charge_cache(n, "pgrc part cache", &mut rec);
                std::hint::spin_loop();
                governor::release_cache(n, &mut rec);
                pairs += 1;
                ran2.store(pairs, Release);
            }
            pairs
        }));
        // the worker's pair count past `seen`, yielding so a loaded host schedules it; bounded, never skipped
        let past = |seen: u64, k: usize| {
            let t0 = std::time::Instant::now();
            while ran.load(Acquire) <= seen {
                assert!(t0.elapsed() < std::time::Duration::from_secs(60), "round {k}: the worker ran no pair in 60 s");
                std::thread::yield_now();
            }
        };
        past(0, 0);
        let mut pairs = 0;
        for k in 0..ROUNDS {
            governor::statement_end();
            governor::statement_start(0, true);
            // this thread allocates no context block here: its count is the slot's share alone
            let mut pin = CacheCharge::default();
            governor::charge_cache(PIN, "pgrc part cache", &mut pin);
            let seen = ran.load(Acquire);
            for _ in 0..64 {
                assert!(l.bytes() >= PIN as isize, "round {k}: a free of another statement's part landed on this one: {} < {PIN}", l.bytes());
                std::hint::spin_loop();
            }
            past(seen, k);
            if k == ROUNDS - 1 {
                stop.store(true, Release);
                pairs = w.take().expect("joined once").join().unwrap();
                assert_eq!(l.bytes(), PIN as isize, "every pair inside this statement netted to nothing");
            }
            governor::release_cache(PIN, &mut pin);
            if k == ROUNDS - 1 {
                assert_eq!(l.bytes(), 0, "the share is exactly 0 again");
            }
        }
        assert!(pairs > 0, "the frees ran");
    });
}

/// A backend taking over a governor slot (`register`) empties its share in a new epoch: a handle that outlived the earlier
/// backend's statement no longer counts it, and that backend's holders' frees take nothing from the new one.
#[test]
fn a_reused_slot_starts_empty() {
    let (old, rec, before) = std::thread::spawn(|| {
        governor::register(4005, 74_005);
        governor::statement_start(0, true);
        let l = governor::current().expect("watch turns the ledger on");
        let before = l.bytes();
        let mut rec = CacheCharge::default();
        governor::charge_cache(N, "pgrc part cache", &mut rec);
        // the backend exits mid-statement: no statement_end
        (l, rec, before)
    })
    .join()
    .unwrap();
    assert_eq!(old.bytes() - before, N as isize, "the share outlives its backend until the slot is taken");
    on(4005, move |l| {
        assert_eq!(old.bytes(), before, "register emptied the share");
        let start = l.bytes();
        free_elsewhere(N, rec);
        assert_eq!(l.bytes(), start, "the earlier backend's free takes nothing from the new backend's statement");
    });
}
