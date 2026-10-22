//! The grant pool (`budget::POOL`): an allocation the pool cannot cover is `Error::OutOfMemory`,
//! never an abort; admission hands out the grant that fits, drawing the statement's reservation
//! down as it allocates; parked regions yield over the park budget. One process-wide pool, so the
//! tests run under one lock and each re-arms it.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{footprint, regions_of, ARENA_ALLOWANCE, try_region, try_region_in, Claimed, Entry, Error, Grant, Open, Step, GRANT_FLOOR, POOL, REGIONS, worker_arena};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

/// D3924: a test's start signal for its holder threads (`holders`, `sessions`), set at its drop too, so a test that fails
/// before it sets `go` still ends them in its unwind and their bytes come back.
struct Go(Arc<std::sync::atomic::AtomicBool>);

impl Go {
    fn new() -> Go {
        Go(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }
}

impl std::ops::Deref for Go {
    type Target = Arc<std::sync::atomic::AtomicBool>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Go {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// One lock for the process-wide pool (D4164's fixture). D3924: a test that failed poisoned it, and its threads give their
/// bytes back only after its unwind (`Go`): the next test waits for them, 30 s at most, before it asserts nothing is held.
fn armed(capacity: u64) -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| {
        LOCK.clear_poison();
        let t0 = std::time::Instant::now();
        while POOL.held() != 0 && t0.elapsed() < Duration::from_secs(30) {
            REGIONS.trim();
            std::thread::sleep(Duration::from_millis(5));
        }
        p.into_inner()
    });
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(capacity);
    g
}

#[test]
fn a_region_over_the_capacity_is_an_error_not_an_abort() {
    let _g = armed(4 * MIB);
    let small = try_region(MIB as usize).expect("1 MiB of 4 fits");
    assert_eq!(POOL.held(), small.capacity() as u64);
    match try_region(8 * MIB as usize) {
        Err(Error::OutOfMemory { bytes, held, capacity, .. }) => {
            assert_eq!(bytes, 8 * MIB);
            assert_eq!(held, small.capacity() as u64);
            assert_eq!(capacity, 4 * MIB);
        }
        other => panic!("expected OutOfMemory, got {:?}", other.map(|r| r.len())),
    }
    assert_eq!(POOL.held(), small.capacity() as u64, "a refused request holds nothing");
    drop(small);
    assert_eq!(POOL.held(), 0, "a dropped region releases its bytes");
    POOL.arm(0);
}

/// D1421: a hosted store's growth charged past G and past the capacity is never refused; the pool counts it as borrowed
/// and holds it until the reservation drops.
#[test]
fn hosted_bytes_are_charged_past_the_grant_never_refused_and_given_back() {
    let _g = armed(16 * MIB);
    let a = POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).unwrap();
    let (reserved, borrows) = (a.reserved, POOL.stats().borrows);
    a.charge_hosted(0);
    assert_eq!((a.hosted(), POOL.held(), POOL.stats().borrows), (0, reserved, borrows), "nothing grew: nothing charged");
    a.charge_hosted(3 * MIB);
    a.charge_hosted(32 * MIB);
    assert_eq!(a.hosted(), 35 * MIB);
    assert_eq!(POOL.held(), reserved + 35 * MIB, "past G and past the capacity: charged, never refused");
    assert_eq!((a.borrowed(), POOL.stats().borrows), (35 * MIB, borrows + 2));
    assert_eq!(a.left(), reserved, "G's room is untouched");
    drop(a);
    assert_eq!(POOL.held(), 0, "the drop gives the reservation and the hosted bytes back");
    POOL.arm(0);
}

/// D2845: a kept hold (a level's mapped admission arena's resident prefix) is taken past the capacity too, never refused;
/// admission sees it, and its drop gives it back. D3804: an ask that does not fit beside the holds runs alone once no
/// statement holds bytes (the holds come back only when their sessions end), and a second one beside it is refused.
#[test]
fn a_kept_hold_is_never_refused_and_admission_sees_it() {
    let _g = armed(16 * MIB);
    let kept = POOL.keep(12 * MIB);
    let more = POOL.keep(8 * MIB);
    assert_eq!(POOL.held(), 20 * MIB, "past the capacity: held, never refused");
    let alone = POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).expect("nothing but the holds is held: alone past the pool");
    assert_eq!(alone.how, gravity_driver::Admitted::Solo, "{alone:?}");
    assert!(POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).is_err(), "nothing fits beside the holds and a running statement");
    drop(alone);
    drop(more);
    drop(kept);
    assert_eq!(POOL.held(), 0, "the drops give the holds back");
    let a = POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).expect("admitted once the holds are gone");
    drop(a);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// A lone statement whose ask does not fit beside kept holds only halves at once: no statement holds bytes, and a kept hold
/// lives for a session, so the short wait could not see any come back. Beside another session's grant the same ask still
/// waits the short wait first (D3242: beside its own caller's grant, a nested ask, it halves at once: d3242_nested_alone.rs).
#[test]
fn a_lone_ask_beside_kept_holds_halves_at_once_and_waits_beside_a_grant() {
    let _g = armed(256 * MIB);
    POOL.set_reserve(0);
    let wait = Duration::from_millis(400);
    let kept = POOL.keep(192 * MIB);
    let a = POOL.admit_need(Grant::of(128 * MIB), &|g| g.bytes, wait).expect("the halving fits beside the hold");
    assert_eq!((a.grant, a.reserved), (Grant::of(128 * MIB).trimmed(64 * MIB), 64 * MIB));
    assert!(a.waited_ms < 100, "kept holds only: no short wait, waited {} ms", a.waited_ms);
    drop(a);
    drop(kept);
    assert_eq!((POOL.held(), POOL.held_by_statements()), (0, 0));
    // another session's statement holds 192 MiB until this one is admitted
    let (tx, rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let held = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(192 * MIB), &|g| g.bytes, wait).expect("fits whole");
        tx.send(()).unwrap();
        done_rx.recv().unwrap();
        drop(r);
    });
    rx.recv().unwrap();
    let b = POOL.admit_need(Grant::of(128 * MIB), &|g| g.bytes, wait).expect("the halving fits beside the grant");
    assert_eq!((b.grant, b.reserved), (Grant::of(128 * MIB).trimmed(64 * MIB), 64 * MIB));
    assert!(b.waited_ms >= 400, "a statement's grant held: the short wait runs out first, waited {} ms", b.waited_ms);
    drop(b);
    done_tx.send(()).unwrap();
    held.join().unwrap();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3069: a pending interrupt during the wait for the whole ask ends the wait as an interrupted refusal, never as a halved
/// grant. An interrupt that raises nothing (an early timer wake: the idle timer's wake outlives the disarm) reached every
/// statement queued 1 s after its ReadyForQuery, and each one halved and spilled: a concurrent analytic run fell 13 -> 4 QPS. Its slot
/// comes back, and queued again with no interrupt the same ask waits for the whole grant.
#[test]
fn an_interrupt_ends_the_queue_wait_without_halving_the_grant() {
    let _g = armed(256 * MIB);
    POOL.set_reserve(0);
    // another statement's grant (this thread's): the waiters below run on their own threads, as other backends do
    let held = POOL.admit_need(Grant::of(192 * MIB), &|g| g.bytes, Duration::from_millis(10)).expect("fits whole");
    let inflight = POOL.stats().inflight;
    let interrupted = std::thread::spawn(|| {
        let t0 = std::time::Instant::now();
        let stop = || t0.elapsed() >= Duration::from_millis(50);
        match POOL.admit_need_until(Grant::of(128 * MIB), &|g| g.bytes, Duration::from_secs(5), &stop) {
            Err(e) => (e.interrupted, e.grant.bytes, e.need, e.waited_ms),
            Ok(a) => panic!("an interrupt took a {} MiB grant of 128 asked", a.grant.bytes / MIB),
        }
    })
    .join()
    .unwrap();
    let (was, grant, need, waited_ms) = interrupted;
    assert!(was, "the interrupt ends the wait");
    assert_eq!((grant, need), (128 * MIB, 128 * MIB), "the whole ask, not a halving");
    assert!(waited_ms < 2000, "ended at the interrupt, not at a stall: {waited_ms} ms");
    assert_eq!((POOL.held(), POOL.stats().inflight), (192 * MIB, inflight), "the interrupted wait holds nothing, its slot came back");
    let queued = std::thread::spawn(|| {
        let a = POOL.admit_need_until(Grant::of(128 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("queued again: admitted");
        (a.grant.bytes, a.reserved)
    });
    std::thread::sleep(Duration::from_millis(200));
    drop(held);
    assert_eq!(queued.join().unwrap(), (128 * MIB, 128 * MIB), "the whole grant once the holder ends");
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn unarmed_pool_bounds_nothing() {
    let _g = armed(0);
    let r = try_region(16 * MIB as usize).expect("unarmed: the allocator decides");
    assert_eq!(POOL.held(), r.capacity() as u64, "unarmed: counted, never refused");
    drop(r);
    assert_eq!(POOL.held(), 0);
}

#[test]
fn admission_reserves_and_the_statement_draws_it_down() {
    let _g = armed(64 * MIB);
    let want = Grant::of(8 * MIB);
    let a = POOL.admit(want, 1, 2, Duration::from_millis(10)).unwrap();
    assert_eq!(a.grant, want);
    assert_eq!(a.reserved, regions_of(want, 1, 2));
    assert_eq!(POOL.held(), a.reserved);
    assert_eq!(a.left(), a.reserved);
    let split = want.split(1, 2);
    let leader = try_region_in(&a, split.breaker.bytes as usize).expect("fits the reservation");
    assert!(leader.is_charged());
    assert_eq!(POOL.held(), a.reserved, "the leader share charged to G: nothing more held");
    assert_eq!(a.left(), a.reserved - leader.capacity() as u64);
    // past G: refused though the capacity has room (S7: never a draw past the grant), nothing taken
    let left = a.left();
    assert!(matches!(try_region_in(&a, 16 * MIB as usize), Err(Error::OutOfMemory { .. })), "past G");
    assert_eq!((POOL.held(), a.left()), (a.reserved, left), "the refusal took nothing");
    let reserved = a.reserved;
    drop(a);
    assert_eq!(POOL.held(), reserved, "a live region charged to G holds G (its room returns with its last charge)");
    drop(leader);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_panic_out_of_a_life_holding_a_free_list_region_releases_its_charge_and_the_grant_ends_at_zero() {
    let _g = armed(64 * MIB);
    REGIONS.put(try_region(MIB as usize).expect("capacity"));
    assert_eq!(POOL.held(), MIB, "a parked region: the pool holds it");
    let a = POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let life = a.life("test life", MIB).expect("admitted");
        let buf = REGIONS.take_for(MIB as usize, Some(&a), Some(&life)).expect("the parked region");
        assert!(buf.is_charged());
        assert_eq!(a.grant_ctx().expect("admitted").subtree_used(), buf.capacity());
        assert_eq!(POOL.held(), a.reserved, "the parked bytes went back once the life took the charge (D191)");
        panic!("unwind out of the life");
    }));
    assert!(r.is_err());
    assert_eq!(a.grant_ctx().expect("admitted").subtree_used(), 0, "the unwind released the life's charge");
    assert_eq!(a.left(), a.reserved);
    drop(a);
    assert_eq!(POOL.held(), 0, "the grant is back to zero after the statement");
    POOL.arm(0);
}

#[test]
fn admission_under_pressure_shrinks_to_what_fits_after_the_wait() {
    let _g = armed(400 * MIB);
    let hog = try_region(200 * MIB as usize).expect("200 of 400");
    // 200 MiB left: a 256 MiB grant maps 512 (leader + one worker), 128 maps 256, 64 maps 128 — the floor fits
    let want = Grant::of(256 * MIB);
    let t0 = std::time::Instant::now();
    let a = POOL.admit(want, 1, 1, Duration::from_millis(30)).unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(30), "waited the bound");
    assert_eq!(a.grant.bytes, GRANT_FLOOR, "down to the floor, the first that fits: {:?}", a);
    assert_eq!(a.reserved, regions_of(a.grant, 1, 1), "what fits is reserved: {:?}", a);
    assert!(a.waited_ms >= 30);
    // the statement's end (its reservation dropped) releases what it reserved; a want that fits after one halving is halved once
    drop(a);
    let want2 = Grant::of(128 * MIB);
    let b = POOL.admit(want2, 1, 1, Duration::from_millis(10)).unwrap();
    assert_eq!(b.grant.bytes, 64 * MIB, "{:?}", b);
    assert_eq!(b.reserved, regions_of(b.grant, 1, 1));
    assert_eq!(POOL.held(), hog.capacity() as u64 + b.reserved);
    // nothing fits at all: the statement waits out its bound at the floor, then is refused by name, holding nothing (S7: never
    // admitted with nothing reserved)
    drop(b);
    let hog2 = try_region(150 * MIB as usize).expect("fits beside the first hog");
    let held = POOL.held();
    let t0 = std::time::Instant::now();
    let c = POOL.admit(want2, 1, 1, Duration::from_millis(40)).expect_err("the floor's need does not fit");
    assert!(t0.elapsed() >= Duration::from_millis(40), "waited its bound at the floor");
    assert_eq!((c.grant.bytes, c.need), (GRANT_FLOOR, regions_of(Grant::of(GRANT_FLOOR), 1, 1)), "{:?}", c);
    assert_eq!(POOL.held(), held, "a refusal holds nothing");
    // a statement that ends inside the bound lets the waiter in at the floor
    let waiter = std::thread::spawn(move || POOL.admit(want2, 1, 1, Duration::from_secs(5)).map(|r| (r.grant, r.reserved)));
    std::thread::sleep(Duration::from_millis(30));
    drop(hog2);
    let (g, r) = waiter.join().unwrap().expect("admitted once the bytes came back");
    assert_eq!((g.bytes, r), (GRANT_FLOOR, regions_of(g, 1, 1)));
    drop(hog);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn the_depot_drops_instead_of_parking_over_the_budget() {
    let _g = armed(16 * MIB);
    let a = try_region(6 * MIB as usize).expect("6 of 16");
    let b = try_region(6 * MIB as usize).expect("12 of 16");
    let before = REGIONS.stats();
    REGIONS.put(a); // held 12 > 8: dropped, not parked
    let mid = REGIONS.stats();
    assert_eq!(mid.entries, before.entries, "over the park budget the region is dropped");
    assert!(POOL.held() <= 6 * MIB + 4096);
    REGIONS.put(b); // held 6 <= 8: parked
    let after = REGIONS.stats();
    assert_eq!(after.entries, before.entries + 1, "under the budget the region parks");
    assert!(POOL.held() >= 6 * MIB, "a parked region stays held");
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "trim releases the parked bytes");
    POOL.arm(0);
}

/// RULE 7: a statement's regions park at its end beside its unspent reservation (`RegionDepot::parking_beside`, from
/// `Entry::release`): the park budget leaves out the room the statement never drew, so a reservation past half the pool
/// parks the regions it drew instead of dropping them (CB9 W8 under DR-12's parallel-owner reservation dropped 8 of 11).
#[test]
fn a_statement_reserved_past_half_the_pool_parks_its_regions_at_its_end() {
    let _g = armed(64 * MIB);
    let a = POOL.admit(Grant::of(20 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
    assert!(a.reserved > 32 * MIB, "the reservation alone passes half the pool: {a:?}");
    let (r, again) = (try_region_in(&a, 4 * MIB as usize).expect("inside G"), try_region_in(&a, 4 * MIB as usize).expect("inside G"));
    let before = REGIONS.stats().entries;
    REGIONS.put(again);
    assert_eq!(REGIONS.stats().entries, before, "counted whole, the unspent reservation drops the region");
    let room = a.left();
    assert!(room > 0 && POOL.held() - room <= 32 * MIB, "room {room} held {}", POOL.held());
    REGIONS.parking_beside(room, || REGIONS.put(r));
    assert_eq!(REGIONS.stats().entries, before + 1, "beside the unspent reservation the region parks");
    drop(a);
    assert_eq!(POOL.held(), 4 * MIB, "G's drop leaves the parked region held");
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// Statements holding `n` x `each` bytes on their own thread, ending one by one `every` apart once `go` is set.
/// D3242: `n` sessions (one thread each) of one `each`-byte statement, the k-th ending `k x every` after `go`. `holders` puts
/// all of them on one thread: one session, whose first statement holds its slot (`RUNNING` counts it once) and ends first;
/// a nested ask's wait reads `RUNNING` for the statements that could give bytes back, one per session.
fn sessions(n: usize, each: u64, every: Duration, go: Arc<std::sync::atomic::AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let hs: Vec<_> = (1..=n as u32)
        .map(|k| {
            let (tx, go) = (tx.clone(), go.clone());
            std::thread::spawn(move || {
                let r = POOL.admit_need(Grant::of(each), &|g| g.bytes, Duration::from_millis(10)).unwrap();
                tx.send(()).unwrap();
                while !go.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(every * k);
                drop(r);
            })
        })
        .collect();
    for _ in 0..n {
        rx.recv().unwrap();
    }
    hs
}

fn holders(n: usize, each: u64, every: Duration, go: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let held: Vec<_> = (0..n).map(|_| POOL.admit_need(Grant::of(each), &|g| g.bytes, Duration::from_millis(10)).unwrap()).collect();
        tx.send(()).unwrap();
        while !go.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        for r in held {
            std::thread::sleep(every);
            drop(r);
        }
    });
    rx.recv().unwrap();
    h
}

#[test]
fn a_waiter_takes_its_whole_ask_while_grants_keep_coming_back() {
    let _g = armed(400 * MIB);
    let go = Go::new();
    let h = holders(4, 100 * MIB, Duration::from_millis(900), go.clone());
    // 300 MiB fit only once three of the four holders ended (~2.7 s), past BYTES_WAIT: one grant back every 0.9 s keeps it
    // waiting for the whole ask, where a 2 s bound would have halved it to 150 MiB at the first grant back
    go.store(true, std::sync::atomic::Ordering::Release);
    let t0 = std::time::Instant::now();
    let a = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_secs(20)).unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(2600), "{:?}", t0.elapsed());
    assert_eq!((a.grant.bytes, a.reserved), (300 * MIB, 300 * MIB), "{a:?}");
    drop(a);
    h.join().unwrap();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// RULE 7: a region whose life ends mid-statement (`RegionDepot::put_from_life`: its charge to the pool, G's room back) parks
/// beside its statement's unspent reservation, which now holds the region's bytes twice over (CB9 W8's slot-0 share).
#[test]
fn a_region_freed_mid_statement_parks_beside_its_statements_room() {
    let _g = armed(64 * MIB);
    let a = POOL.admit(Grant::of(20 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
    let (r, again) = (try_region_in(&a, 4 * MIB as usize).expect("inside G"), try_region_in(&a, 4 * MIB as usize).expect("inside G"));
    let before = REGIONS.stats().entries;
    REGIONS.put_from_life(again, None);
    assert_eq!(REGIONS.stats().entries, before, "without its statement the room counts and the region drops");
    REGIONS.put_from_life(r, Some(&a));
    assert_eq!(REGIONS.stats().entries, before + 1, "beside its statement's room the region parks");
    drop(a);
    assert_eq!(POOL.held(), 4 * MIB, "the parked region alone stays held");
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_later_admission_leaves_the_waiters_need_free() {
    let _g = armed(400 * MIB);
    let go = Go::new();
    let h = holders(1, 300 * MIB, Duration::from_millis(400), go.clone());
    let waiter = std::thread::spawn(|| {
        // D3924: a loaded box starts the waiter late (S1 on shared slot 19s2); 200 ms here stands in for that
        std::thread::sleep(Duration::from_millis(200));
        POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_secs(20)).map(|r| r.grant.bytes)
    });
    // D3924: the waiter holds the turn (a 50 ms sleep was the premise; on a loaded box the waiter had not queued yet)
    let t0 = std::time::Instant::now();
    while POOL.turn_need() != 300 * MIB {
        assert!(t0.elapsed() < Duration::from_secs(20), "the waiter never took the turn");
        std::thread::sleep(Duration::from_millis(1));
    }
    // 100 MiB are free, but the waiter's 300 MiB are kept: a 50 MiB statement arriving after it waits its bound and is refused
    let late = POOL.admit_need(Grant::of(50 * MIB), &|g| g.bytes, Duration::from_millis(40)).expect_err("the waiter's need is kept free");
    assert_eq!(late.need, 50 * MIB, "{late:?}");
    go.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(waiter.join().unwrap().expect("admitted once the holder ended"), 300 * MIB, "the whole ask");
    h.join().unwrap();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3924: a test that fails while its holder threads hold bytes poisons the lock, and its `Go` guard ends them in the unwind;
/// the next test arms the pool once their bytes are back instead of failing on them (one failure cascaded into 34 more).
#[test]
fn a_failed_test_leaves_no_bytes_for_the_next_one() {
    let failed = std::thread::spawn(|| {
        let _g = armed(400 * MIB);
        let go = Go::new();
        let _h = holders(1, 300 * MIB, Duration::from_millis(200), go.clone());
        assert_eq!(POOL.held(), 300 * MIB);
        panic!("a planted failure while a holder thread holds 300 MiB");
    })
    .join();
    assert!(failed.is_err(), "the planted failure");
    let _g = armed(400 * MIB);
    assert_eq!(POOL.held(), 0, "its holder's bytes came back");
    POOL.arm(0);
}

#[test]
fn a_turn_holder_blocks_no_running_draw_and_takes_its_whole_ask_when_the_drawer_ends() {
    let _g = armed(400 * MIB);
    let go = Go::new();
    let (tx, rx) = std::sync::mpsc::channel();
    // a running statement on another thread holds 300 MiB; the waiter here asks 200 MiB, takes the turn and waits
    let runner = {
        let go = go.clone();
        std::thread::spawn(move || {
            let s = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_millis(10)).unwrap();
            tx.send(()).unwrap();
            while !go.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(200));
            // its run-time draw past its grant (a region from the capacity) while the waiter holds the turn: 300 + 80 fits the
            // 400, and the turn's 200 does not hold it back (a draw waiting on the waiter that waits on it would deadlock)
            let r = try_region(80 * MIB as usize);
            let drew = r.is_ok();
            std::thread::sleep(Duration::from_millis(100));
            drop(r);
            drop(s);
            drew
        })
    };
    rx.recv().unwrap();
    go.store(true, std::sync::atomic::Ordering::Release);
    let t0 = std::time::Instant::now();
    let a = POOL.admit_need(Grant::of(200 * MIB), &|g| g.bytes, Duration::from_secs(20)).unwrap();
    let waited = t0.elapsed();
    assert!(runner.join().unwrap(), "a running draw is never held back by a waiter's turn");
    // the runner's grant came back (~300 ms): the whole ask, before any halving
    assert!(waited >= Duration::from_millis(250) && waited < Duration::from_millis(1900), "{waited:?}");
    assert_eq!((a.grant.bytes, a.reserved), (200 * MIB, 200 * MIB), "{a:?}");
    drop(a);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn queued_waiters_take_their_whole_asks_oldest_first() {
    let _g = armed(400 * MIB);
    let go = Go::new();
    let h = holders(1, 300 * MIB, Duration::from_millis(200), go.clone());
    go.store(true, std::sync::atomic::Ordering::Release);
    // four 300 MiB waiters queue one after another; each fits only alone beside the 100 MiB left, so each waits for the one before
    let order = Arc::new(AtomicU64::new(0));
    let (base, trims) = (POOL.stats().waits, POOL.stats().trims);
    let mut ws = Vec::new();
    for i in 0..4u64 {
        let order_i = order.clone();
        ws.push(std::thread::spawn(move || {
            let a = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_secs(20)).unwrap();
            let nth = order_i.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let whole = a.grant.bytes;
            std::thread::sleep(Duration::from_millis(100));
            drop(a);
            (i, nth, whole)
        }));
        // D3123: waiter i+1 starts only once waiter i is in the queue (the pool counted its wait and its entry's depot trim;
        // its ticket follows within a few loads, hence the margin) or already admitted: the queue order is the test's, not the
        // scheduler's
        let t0 = std::time::Instant::now();
        let queued = |s: gravity_driver::PoolStats| s.waits - base >= i + 1 && s.trims - trims >= i + 1;
        while !queued(POOL.stats()) && order.load(std::sync::atomic::Ordering::Acquire) < i + 1 {
            assert!(t0.elapsed() < Duration::from_secs(20), "waiter {i} never reached the queue");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for w in ws {
        let (i, nth, whole) = w.join().unwrap();
        assert_eq!((i, whole), (nth, 300 * MIB), "waiter {i} admitted {nth}th: the oldest waiter holds the turn, and takes its whole ask");
    }
    h.join().unwrap();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_nested_waiter_takes_no_turn_and_halves_after_the_short_wait_while_others_end() {
    let _g = armed(420 * MIB);
    // a statement on this thread holds 300 MiB; its nested statement here asks 200 MiB, which can never fit beside its
    // caller's grant (that comes back only after it)
    let caller = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_millis(10)).unwrap();
    let go = Go::new();
    let h = sessions(6, 10 * MIB, Duration::from_millis(500), go.clone());
    // a third statement while the nested one waits: no turn holds its 10 MiB back (350 + 10 of 420)
    let other = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(700));
        POOL.admit_need(Grant::of(10 * MIB), &|g| g.bytes, Duration::from_millis(10)).map(drop).is_ok()
    });
    go.store(true, std::sync::atomic::Ordering::Release);
    let t0 = std::time::Instant::now();
    let nested = POOL.admit_need(Grant::of(200 * MIB), &|g| g.bytes, Duration::from_secs(20)).unwrap();
    let waited = t0.elapsed();
    assert!(other.join().unwrap(), "a nested waiter holds no turn");
    // grants keep coming back (a holder every 0.5 s to 3 s) but none can make room beside the caller: the short wait (2 s)
    // ends and it halves to what fits, never stalling the pool on its caller
    assert!(waited >= Duration::from_millis(1900) && waited < Duration::from_millis(3500), "{waited:?}");
    assert_eq!(nested.grant.bytes, 100 * MIB, "{nested:?}");
    for t in h {
        t.join().unwrap();
    }
    drop(nested);
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// A kept hold lives for a session and no statement's end gives it back: a lone statement whose need fits only without it
/// takes no turn and halves at once, with neither the short wait nor the queue's stall (bar's TH18 / CB W8 2 s wait).
#[test]
fn a_lone_need_that_fits_only_without_the_kept_holds_halves_at_once_however_long_its_bound() {
    let _g = armed(400 * MIB);
    POOL.set_reserve(0);
    let kept = POOL.keep(150 * MIB);
    let t0 = std::time::Instant::now();
    let a = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_secs(60)).expect("the halving fits beside the hold");
    let waited = t0.elapsed();
    assert!(waited < Duration::from_millis(100), "kept holds only: nothing to wait for, waited {waited:?}");
    assert_eq!((a.grant, a.reserved), (Grant::of(300 * MIB).trimmed(150 * MIB), 150 * MIB));
    drop(a);
    drop(kept);
    assert_eq!((POOL.held(), POOL.held_by_statements()), (0, 0));
    POOL.arm(0);
}

/// A queued need the kept holds outgrow while it waits gives the turn up and halves after the short wait, as a need that
/// never fit beside them does, not after the queue's stall: no turnover can make room for it while the holds stay.
#[test]
fn a_queued_need_the_kept_holds_outgrow_gives_the_turn_up_and_halves_after_the_short_wait() {
    let _g = armed(400 * MIB);
    POOL.set_reserve(0);
    let go = Go::new();
    let h = holders(1, 150 * MIB, Duration::from_millis(100), go.clone());
    let waiter = std::thread::spawn(|| {
        let t0 = std::time::Instant::now();
        let a = POOL.admit_need(Grant::of(300 * MIB), &|g| g.bytes, Duration::from_secs(60)).map(|r| r.grant.bytes);
        (a, t0.elapsed())
    });
    std::thread::sleep(Duration::from_millis(100));
    // 300 fit the 400 whole once the holder ends: it queued with the turn; with 150 kept they never can
    let kept = POOL.keep(150 * MIB);
    let (a, waited) = waiter.join().unwrap();
    assert!(waited >= Duration::from_millis(1900) && waited < Duration::from_millis(2900), "{waited:?}");
    assert_eq!(a.expect("the halving fits beside the holder and the hold"), 75 * MIB);
    go.store(true, std::sync::atomic::Ordering::Release);
    h.join().unwrap();
    drop(kept);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3804: a floor need the big lane seats when empty but not beside the kept holds (each admitting session's admission-arena
/// prefix, D2845, which no statement's end gives back) is past the lane: with nothing else held it runs alone past the
/// pool at once (D3380's road), where it waited out its whole bound (600 s by default) and ended out of memory; a need
/// that fits beside the kept holds is admitted in the lane as before, and a past-the-lane floor still waits while another
/// statement holds bytes that could come back.
#[test]
fn a_floor_inside_the_kept_holds_runs_alone_at_once_instead_of_waiting_its_bound() {
    let _g = armed(64 * MIB);
    // two sessions' admission arenas: 2 x 16 MiB here (64 MiB each in a server)
    let kept = [POOL.keep(16 * MIB), POOL.keep(16 * MIB)];
    let want = Grant::of(GRANT_FLOOR);
    // a floor need of 48 MiB: under the 64 MiB lane, past it beside the 32 MiB kept
    let need = |_: Grant| 48 * MIB;
    let t0 = std::time::Instant::now();
    let a = POOL.admit_need(want, &need, Duration::from_secs(5)).expect("nothing else is held: alone past the pool");
    assert!(t0.elapsed() < Duration::from_secs(1), "at once, not after its 5 s bound: {:?}", t0.elapsed());
    assert_eq!(a.how, gravity_driver::Admitted::Solo, "{a:?}");
    assert_eq!(POOL.held(), 80 * MIB, "the floor's need held past the lane beside the kept holds");
    // while it runs, a second such floor waits for its bytes (here its 300 ms bound)
    let other = std::thread::spawn(move || POOL.admit_need(want, &need, Duration::from_millis(300)).map(|r| r.how).map_err(|e| e.need));
    assert_eq!(other.join().unwrap(), Err(48 * MIB), "a second floor past the lane waits while the first holds bytes");
    drop(a);
    assert_eq!(POOL.held(), 32 * MIB, "the kept holds alone");
    // a need that fits beside the kept holds is admitted in the lane, whole
    let fits = POOL.admit_need(want, &|_: Grant| 24 * MIB, Duration::from_secs(5)).expect("fits beside the kept holds");
    assert_eq!(fits.how, gravity_driver::Admitted::Whole, "{fits:?}");
    drop(fits);
    drop(kept);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3380's bound: a floor need past the memory governor's bar itself can never be held, whatever
/// is resident, so it is never admitted alone: it refuses at once (no wait, no turn taken), and a want that fits beside it
/// is admitted at once too. Under the bar but past what the resident bytes leave, it waits (D3380's solo wait); under the
/// bar with room, the same want runs alone past the pool.
#[test]
fn a_floor_need_past_the_governors_bar_refuses_at_once_and_takes_no_turn() {
    let _g = armed(16 * MIB);
    let want = Grant::of(256 * MIB);
    let need = |g: Grant| g.bytes;
    let disarm = || mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
    mcx::governor::publish((GRANT_FLOOR / 2) as usize, 0, mcx::global_footprint::bytes());
    let t0 = std::time::Instant::now();
    let r = POOL.admit_need(want, &need, Duration::from_secs(5));
    let took = t0.elapsed();
    let fits = POOL.admit_need(Grant::of(MIB), &need, Duration::from_millis(300)).map(|r| r.grant.bytes);
    disarm();
    let r = r.expect_err("a floor need past the bar is never held");
    assert_eq!((r.grant.bytes, r.need), (GRANT_FLOOR, GRANT_FLOOR), "{r:?}");
    assert!(took < Duration::from_secs(1), "refused at once, not after its 5 s wait: {took:?}");
    assert_eq!(fits, Ok(MIB), "it took no turn: a want that fits is admitted at once");
    assert_eq!(POOL.held(), 0);
    // under the bar but past what the resident bytes leave: the solo wait (here its 300 ms bound), then the refusal
    mcx::governor::publish((GRANT_FLOOR * 4) as usize, (GRANT_FLOOR * 7 / 2) as usize, mcx::global_footprint::bytes());
    let t0 = std::time::Instant::now();
    let w = POOL.admit_need(want, &need, Duration::from_millis(300));
    let waited = t0.elapsed();
    assert!(w.is_err() && waited >= Duration::from_millis(300), "a need under the bar waits its bound: {waited:?} {:?}", w.map(|r| r.how));
    assert_eq!(POOL.held(), 0);
    mcx::governor::publish((GRANT_FLOOR * 4) as usize, 0, mcx::global_footprint::bytes());
    let a = POOL.admit_need(want, &need, Duration::from_secs(5));
    disarm();
    let a = a.expect("under the bar: alone past the pool");
    assert_eq!(a.how, gravity_driver::Admitted::Solo);
    drop(a);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3380: a want whose floor no capacity here could ever hold runs alone past the pool: admitted at once while nothing is
/// held (`Admitted::Solo`, its floor's need held past the capacity); while it runs a second such want and a want that fits
/// both wait (here to their bound); once it ends the next one runs alone in turn and the pool is empty after it.
#[test]
fn a_want_no_capacity_could_ever_hold_runs_alone_past_the_pool() {
    let _g = armed(16 * MIB);
    let want = Grant::of(256 * MIB);
    let t0 = std::time::Instant::now();
    let a = POOL.admit(want, 1, 1, Duration::from_secs(5)).expect("nothing is held: alone past the pool");
    assert!(t0.elapsed() < Duration::from_secs(1), "nothing is held: no wait");
    assert_eq!((a.grant.bytes, a.how), (GRANT_FLOOR, gravity_driver::Admitted::Solo), "{a:?}");
    assert!(POOL.held() > 16 * MIB, "the floor's need is held past the capacity: {}", POOL.held());
    let other = std::thread::spawn(move || POOL.admit(want, 1, 1, Duration::from_millis(300)).map(|r| r.how).map_err(|e| e.grant.bytes));
    assert_eq!(other.join().unwrap(), Err(GRANT_FLOOR), "a second floor past the pool waits while the first runs alone");
    let fits = std::thread::spawn(move || POOL.admit(Grant::of(MIB), 1, 1, Duration::from_millis(300)).map(|r| r.grant.bytes).map_err(|e| e.grant.bytes));
    assert!(fits.join().unwrap().is_err(), "a want that fits waits too: the pool is past its lane");
    drop(a);
    assert_eq!(POOL.held(), 0, "the solo's need comes back whole");
    let b = POOL.admit(want, 1, 1, Duration::from_secs(5)).expect("alone in turn once the first ended");
    assert_eq!(b.how, gravity_driver::Admitted::Solo);
    drop(b);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn the_gate_bounds_statements_in_flight_and_a_slot_frees_at_the_statement_end() {
    let _g = armed(1 << 30);
    POOL.gate(1);
    let want = Grant::of(8 * MIB);
    let a = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    assert_eq!(a.grant, want);
    assert_eq!(POOL.inflight(), 1);
    // a second statement on another thread waits the bound, then runs anyway (bounded latency)
    let t = std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let b = POOL.admit(want, 1, 1, Duration::from_millis(50)).unwrap();
        let waited = t0.elapsed();
        assert!(!b.holds_slot(), "the gated thread took no slot");
        (b.grant, waited)
    });
    let (b, waited) = t.join().unwrap();
    assert!(waited >= Duration::from_millis(50), "waited the bound: {waited:?}");
    assert_eq!(b, want);
    assert_eq!(POOL.inflight(), 1, "the gated thread took no slot");
    drop(a);
    assert_eq!(POOL.inflight(), 0);
    // with the first statement over, the next takes the slot at once
    let t = std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let c = POOL.admit(want, 1, 1, Duration::from_millis(500)).unwrap();
        let waited = t0.elapsed();
        assert!(c.holds_slot());
        (c.grant, waited)
    });
    let (c, waited) = t.join().unwrap();
    assert!(waited < Duration::from_millis(100), "{waited:?}");
    assert_eq!(c, want);
    assert_eq!(POOL.inflight(), 0, "the thread's reservation gave its slot back");
    POOL.gate(0);
    POOL.arm(0);
}

#[test]
fn gated_the_depot_keeps_regions_for_admission_to_trim() {
    // the depot parks under the budget; admission's trim frees it — the path a gated park's regions take
    let _g = armed(64 * MIB);
    POOL.gate(2);
    let r = try_region(8 * MIB as usize).expect("8 of 64");
    REGIONS.put(r);
    assert!(POOL.held() >= 8 * MIB, "parked in the depot, still held");
    let hog = try_region(44 * MIB as usize).expect("44 of 64 beside the parked 8");
    // 12 MiB left: a want of 8 MiB (regions 16) does not fit until the depot is trimmed, which admit does
    let a = POOL.admit(Grant::of(8 * MIB), 1, 1, Duration::from_millis(50)).unwrap();
    assert_eq!((a.grant.bytes, a.reserved), (8 * MIB, 16 * MIB), "{:?}", a);
    assert_eq!(REGIONS.stats().entries, 0, "the depot was trimmed");
    drop(a);
    drop(hog);
    assert_eq!(POOL.held(), 0);
    POOL.gate(0);
    POOL.arm(0);
}

/// An allocation past the statement's reservation trims the parked capacity of previous statements
/// (the region and slot depots) before the pool refuses it: 2026-09-19, a two-key group-by's leader region
/// refused mid-statement beside 8 GiB of parked worker slots (`UNSUPPORTED_SCRATCH` on one server).
#[test]
fn an_allocation_past_the_reservation_trims_the_depots_before_a_refusal() {
    let _g = armed(16 * MIB);
    let a = try_region(6 * MIB as usize).expect("6 of 16");
    REGIONS.put(a); // held 6 <= 8: parked
    assert_eq!(REGIONS.stats().entries, 1);
    let big = try_region(12 * MIB as usize).expect("the parked 6 MiB yield: 12 of 16 after the trim");
    assert_eq!(REGIONS.stats().entries, 0, "the depot was trimmed for the allocation");
    assert!(POOL.held() >= 12 * MIB && POOL.held() <= 12 * MIB + 4096, "held {}", POOL.held());
    drop(big);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(5, std::sync::atomic::Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

/// The slot depot yields to the grant pool (2026-09-19: a regexp group-by's and a text group-by's worker
/// slots, 8 GiB parked across fresh backends, left a two-key group-by's leader region refused mid-statement): an
/// allocation the capacity cannot hold beside the parked slots trims them, oldest set first, and succeeds.
#[test]
fn parked_slots_hold_images_only_and_their_regions_park_in_the_free_list() {
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let _g = armed(0);
    let mut cycle = |salt: u64| {
        let mut sink = LegSink::new();
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        exec.set_slot_salt(Some(salt));
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, 3).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..3 {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &CountMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    };
    let s0 = gravity_driver::DEPOT.stats();
    cycle(0x5e_ac_0919);
    cycle(0x5e_ac_0920);
    let s1 = gravity_driver::DEPOT.stats();
    assert_eq!(s1.entries, s0.entries + 2, "two sets parked (unarmed pool: no pressure)");
    // the sets are images only: their share / fixed buffers and chunks went to the free list at the statement's end
    assert_eq!(gravity_driver::POOL.held(), REGIONS.stats().bytes as u64, "every held byte is a parked region, none a parked set's");
    REGIONS.trim();
    assert_eq!(gravity_driver::POOL.held(), 0);
    gravity_driver::DEPOT.trim();
    POOL.arm(0);
}

#[test]
fn two_statements_on_a_thread_own_their_reservations_and_end_in_any_order() {
    let _g = armed(64 * MIB);
    POOL.gate(1);
    let want = Grant::of(8 * MIB);
    for outer_first in [false, true] {
        let outer = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
        assert!(outer.holds_slot() && outer.left() == outer.reserved);
        assert_eq!(POOL.inflight(), 1);
        // a second statement on the thread (a nested host call, or a second cursor): its own reservation, no second slot
        let inner = POOL.admit(Grant::of(4 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
        assert!(!inner.holds_slot(), "one slot per thread, the first statement's");
        assert_eq!(inner.reserved, regions_of(Grant::of(4 * MIB), 1, 1));
        assert_eq!(POOL.held(), outer.reserved + inner.reserved, "both held");
        let split = want.split(1, 1);
        let leader = try_region_in(&outer, split.breaker.bytes as usize).expect("fits the outer's reservation");
        assert_eq!((outer.left(), inner.left()), (outer.reserved - split.breaker.bytes as u64, inner.reserved), "each draws from its own");
        drop(leader);
        // D188: the order they end in changes nothing
        if outer_first {
            drop(outer);
            drop(inner);
        } else {
            drop(inner);
            drop(outer);
        }
        assert_eq!((POOL.inflight(), POOL.held()), (0, 0));
    }
    POOL.gate(0);
    POOL.arm(0);
}

// the small lane (`gravity_grant_mode=lanes`): a 1 GiB pool with a 512 MiB slice; a floor grant's regions at live 1 / width 1 are 128 MiB
fn laned() -> Laned {
    let g = armed(1024 * MIB);
    POOL.set_reserve(512 * MIB);
    assert_eq!(POOL.reserve(), 512 * MIB);
    assert_eq!(regions_of(Grant::of(GRANT_FLOOR), 1, 1), 128 * MIB);
    Laned(g)
}

/// The lane's slice given back however the test ends: a failed lane test leaves no slice for the next one to trip on.
struct Laned(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Drop for Laned {
    fn drop(&mut self) {
        unlaned();
    }
}

fn unlaned() {
    POOL.set_reserve(0);
    POOL.arm(0);
}

#[test]
fn the_small_lane_admits_at_once_beside_a_full_big_lane_and_gives_its_slice_back() {
    let _g = laned();
    let big = try_region(480 * MIB as usize).expect("480 of the big lane's 512 fits");
    assert!(try_region(64 * MIB as usize).is_err(), "the big lane never reaches into the slice");
    let want = Grant::of(GRANT_FLOOR);
    let a = POOL.admit_small(want, 1, 1, false).expect("the slice is empty: admitted at once");
    assert!(a.small && a.waited_ms == 0 && a.grant == want && a.reserved == 128 * MIB);
    assert_eq!(POOL.small_held(), 128 * MIB);
    assert_eq!(POOL.held(), big.capacity() as u64 + 128 * MIB, "held = the big region + the small reservation");
    assert!(a.small && a.left() == 128 * MIB);
    let more: Vec<_> = (0..3).map(|_| POOL.admit_small(want, 1, 1, false).expect("three more fill the slice")).collect();
    assert_eq!(POOL.small_held(), 512 * MIB);
    let before = POOL.stats();
    assert!(POOL.admit_small(want, 1, 1, false).is_none(), "the slice full: the caller goes the big lane's way");
    let after = POOL.stats();
    assert_eq!((after.small_fallbacks, after.held, after.small_held, after.small_admits), (before.small_fallbacks + 1, before.held, before.small_held, before.small_admits));
    assert_eq!(a.left(), 128 * MIB, "nothing of the statement's state moved");
    drop(more);
    drop(a);
    assert_eq!(POOL.small_held(), 0, "the slice is whole again at the statements' end");
    assert_eq!(POOL.held(), big.capacity() as u64);
    drop(big);
    assert_eq!(POOL.held(), 0);
    unlaned();
}

#[test]
fn a_small_statement_past_its_reservation_is_refused_and_counted() {
    let _g = laned();
    let a = POOL.admit_small(Grant::of(GRANT_FLOOR), 1, 1, false).expect("admitted");
    let before = POOL.stats();
    let r1 = try_region_in(&a, 64 * MIB as usize).expect("from the reservation");
    assert_eq!(POOL.stats().small_overflows, before.small_overflows);
    // S7: past its reservation a small statement is refused, though the big lane has room; the overflow is counted
    assert!(matches!(try_region_in(&a, 128 * MIB as usize), Err(Error::OutOfMemory { .. })), "past the reservation");
    assert_eq!(POOL.stats().small_overflows, before.small_overflows + 1);
    assert_eq!(POOL.held(), 128 * MIB, "held = the slice's reservation, r1 charged inside it");
    assert_eq!(POOL.small_held(), 128 * MIB, "the slice by statement");
    drop(r1);
    drop(a);
    assert_eq!(POOL.small_held(), 0);
    assert_eq!(POOL.held(), 0);
    unlaned();
}

#[test]
fn a_big_admission_trims_to_the_big_lane_while_the_slice_stays_free() {
    let _g = laned();
    // 152 MiB of the big lane left: a 256 MiB grant maps 512, 128 maps 256, the floor 128 fits
    let big = try_region(360 * MIB as usize).expect("fits the big lane");
    let a = POOL.admit(Grant::of(256 * MIB), 1, 1, Duration::from_millis(20)).unwrap();
    assert!(!a.small);
    assert!(a.grant.bytes < 256 * MIB, "trimmed: the slice did not count as room ({} MiB)", a.grant.bytes >> 20);
    assert_eq!((a.grant.bytes, a.reserved), (GRANT_FLOOR, 128 * MIB), "the floor, reserved in the big lane");
    assert!(POOL.held() - POOL.small_held() <= 512 * MIB, "the big lane's account");
    let small = POOL.admit_small(Grant::of(GRANT_FLOOR), 1, 1, false).expect("the slice admits regardless of the big lane's state");
    assert_eq!(POOL.small_held(), 128 * MIB);
    drop(small);
    drop(a);
    drop(big);
    assert_eq!((POOL.held(), POOL.small_held()), (0, 0));
    unlaned();
}

#[test]
fn set_reserve_never_takes_more_than_half_the_capacity_and_zero_means_no_lane() {
    let _g = armed(64 * MIB);
    POOL.set_reserve(60 * MIB);
    assert_eq!(POOL.reserve(), 32 * MIB);
    // DR-13 (keep): half of any capacity, whatever the asked slice (an odd capacity rounds down, never past half)
    POOL.arm(64 * MIB + 1);
    POOL.set_reserve(u64::MAX);
    assert_eq!(POOL.reserve(), 32 * MIB);
    POOL.set_reserve(MIB);
    assert_eq!(POOL.reserve(), MIB);
    POOL.arm(64 * MIB);
    POOL.set_reserve(0);
    assert!(POOL.admit_small(Grant::of(GRANT_FLOOR), 1, 1, false).is_none(), "no lane: the caller goes the big lane's way");
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_share_just_over_a_power_of_two_draws_its_bytes_not_its_class() {
    let _g = armed(64 * MIB);
    let share = 9 * MIB as usize + 1;
    let r = REGIONS.take(share, None).expect("9 MiB of 64");
    assert!(r.len() >= share && r.len() <= share + 4096, "D157: a region is its bytes page-rounded, drew {}", r.len());
    assert_eq!(POOL.held(), r.len() as u64);
    let len = r.len();
    REGIONS.put(r);
    let before = REGIONS.stats();
    let again = REGIONS.take(9 * MIB as usize, None).expect("parked");
    // D3074 / D3089: the parked region serves the take it fits, fitted to the ask (its last page an uncharged tail)
    assert_eq!((again.whole(), REGIONS.stats().takes), (len, before.takes + 1), "a parked region of the class under serves a take it fits");
    assert_eq!(again.len(), 9 * MIB as usize, "fitted to the ask");
    assert_eq!(POOL.held(), 9 * MIB as u64, "charged the ask");
    REGIONS.put(again);
    let wide = REGIONS.take(12 * MIB as usize, None).expect("12 of 64");
    assert_eq!(wide.len(), 12 * MIB as usize, "a parked region shorter than the take is never handed out");
    drop(wide);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn the_leader_regions_of_a_grant_fit_its_reservation() {
    let _g = armed(256 * MIB);
    // 3 live breakers over 100 MiB: a 33.3 MiB share each, 64 MiB each at the power-of-two class (D157 drew 2x its grant)
    let want = Grant::of(100 * MIB);
    let a = POOL.admit(want, 3, 1, Duration::from_millis(10)).unwrap();
    assert_eq!(a.grant, want);
    let split = want.split(3, 1);
    let held = POOL.held();
    let regions: Vec<_> = (0..3).map(|_| REGIONS.take(split.breaker.bytes as usize, Some(&a)).expect("fits")).collect();
    assert!(a.left() < split.worker.bytes as u64 + 3 * 4096, "the leader regions drew the reservation down: {} left", a.left());
    assert_eq!(POOL.held(), held, "no leader region drew past the reservation");
    assert!(a.reserved <= want.bytes + split.worker.bytes as u64 + 3 * 4096, "reserved {} for a {} grant", a.reserved, want.bytes);
    drop(a);
    drop(regions);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// One serial run of `program` at `grant` on `entry` (a parked Node under `key` reopens its slot); the leader region's address.
fn serial_run(entry: &mut Entry, program: &Arc<gravity_program::Program>, derive: &mut FakeDerive, key: gravity_program::PlanKey, grant: Grant) -> usize {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(key), program, derive, &mut b, lease(grant)).unwrap();
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap(), Step::Done);
    let at = exec.node().slot_region(0).as_ptr() as usize;
    assert_eq!(exec.node().slot_bytes(0), u32::MAX & !7, "the share at the u32 cap (a multiple of 8, D335)");
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted().len(), 8, "eight groups of v % 8");
    at
}

#[test]
fn a_slot_region_of_4_gib_and_past_it_is_reused_at_every_reopen() {
    use gravity_program::{Generation, Handle, PlanKey};
    let _g = armed(0);
    let program = Arc::new(p_group_share(64));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    // a share at the u32 cap maps a region of exactly 2^32 (D157: its length read as u32 was 0, every reopen re-took 4 GiB)
    let huge = Grant::of(1 << 40);
    let mut entry = Entry::new(2);
    let key = PlanKey { handle: Handle(157), generation: Generation(1) };
    let s0 = REGIONS.stats();
    let at = serial_run(&mut entry, &program, &mut derive, key, huge);
    let s1 = REGIONS.stats();
    assert_eq!((s1.allocs, POOL.held() >= 1 << 32, POOL.held() < (1 << 32) + (64 << 20)), (s0.allocs + 1, true, true), "one region of 2^32, not a larger class");
    assert_eq!(serial_run(&mut entry, &program, &mut derive, key, huge), at);
    assert_eq!((REGIONS.stats().allocs, REGIONS.stats().takes, REGIONS.stats().puts), (s1.allocs, s1.takes + 1, s1.puts + 1), "the reopen takes its 4 GiB region back from the free list: nothing allocated");
    drop(entry);
    // a parked region of 2^32 + 4 KiB serves a fresh Node's 2^32 open and every reopen after it
    REGIONS.trim();
    let parked = try_region((1 << 32) + 4096).expect("unarmed");
    let parked_at = parked.as_ptr() as usize;
    REGIONS.put(parked);
    let mut entry = Entry::new(2);
    let s2 = REGIONS.stats();
    assert_eq!(serial_run(&mut entry, &program, &mut derive, key, huge), parked_at, "the parked region is taken");
    let s3 = REGIONS.stats();
    assert_eq!((s3.allocs, s3.takes), (s2.allocs, s2.takes + 1), "nothing allocated");
    assert_eq!(serial_run(&mut entry, &program, &mut derive, key, huge), parked_at);
    assert_eq!((REGIONS.stats().allocs, REGIONS.stats().takes, REGIONS.stats().puts), (s3.allocs, s3.takes + 1, s3.puts + 1), "a region past 2^32 is reused at the reopen");
    drop(entry);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
}

#[test]
fn a_statement_draws_its_reservation_and_no_more_when_a_dead_slot_gives_its_region_to_the_next_breaker() {
    let _g = armed(1 << 30);
    let program = Arc::new(p_group_chain(64));
    assert_eq!((program.slots.max_live, program.slots.peak[0], program.slots.peak[2]), (2, 2, 2));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let want = Grant::of(64 * MIB);
    let a = std::rc::Rc::new(POOL.admit(want, program.slots.max_live, 1, Duration::from_millis(10)).unwrap());
    let s0 = REGIONS.stats();
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted().len(), 8, "eight groups of v % 8");
    // D157 (a W16 distinct count drew 4 leader shares with 3 live): slot 2 takes dead slot 0's region, two regions for two live slots
    assert_eq!(REGIONS.stats().allocs - s0.allocs, 2, "two leader regions for a peak of two live slots");
    assert!(a.drawn() <= a.reserved, "drawn {} past the reservation {}", a.drawn(), a.reserved);
    drop(a);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3226 (c1): a shared slot's region the full pool refuses at its share (past G: granted 128 MiB, reserved 4 MiB) opens
/// at the pool's room left, armed at it, and the breaker runs there; the refusal was the statement's 53200.
#[test]
fn a_shared_region_the_full_pool_refuses_opens_at_the_room_left() {
    use std::sync::atomic::Ordering::Relaxed;
    let _g = armed(256 * MIB);
    let program = Arc::new(p_group_share(64));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let a = std::rc::Rc::new(POOL.admit_need(Grant::of(128 * MIB), &|_| 4 * MIB, Duration::from_millis(10)).unwrap());
    let fill = POOL.keep(POOL.capacity() - POOL.held() - 16 * MIB);
    let opened = gravity_driver::mem::region::OPENED_SMALLER.load(Relaxed);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted().len(), 8, "eight groups of v % 8");
    assert_eq!(gravity_driver::mem::region::OPENED_SMALLER.load(Relaxed) - opened, 1, "opened at the room left");
    assert!(a.grown() <= 16 * MIB, "the region past G is the room left: {} grown", a.grown());
    assert_eq!(a.refused(), 0, "the refused open is served: no named refusal kept");
    drop(fill);
    drop(a);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn two_statements_whose_grants_fit_but_whose_reservations_do_not_never_both_hold_their_reservation() {
    // admission takes the reservation (`regions_for`: the leader's live shares + the workers'), never the bare grant,
    // so the pool's capacity bounds what statements in flight can draw (the D157 bound: drawn <= reserved)
    let want = Grant::of(256 * MIB);
    let need = regions_of(want, 3, 16);
    assert!(need > want.bytes, "the workers' share rides beside the leader's");
    let _g = armed(600 * MIB);
    assert!(2 * want.bytes <= 600 * MIB && 2 * need > 600 * MIB);
    let a = POOL.admit(want, 3, 16, Duration::from_millis(10)).unwrap();
    assert_eq!((a.grant, a.reserved, POOL.held()), (want, need, need));
    let b = std::thread::spawn(move || {
        let b = POOL.admit(want, 3, 16, Duration::from_millis(20)).unwrap();
        let held = POOL.held();
        ((b.grant, b.reserved), held)
    })
    .join()
    .unwrap();
    let (b, held) = b;
    assert!(b.0.bytes < want.bytes, "the second statement runs trimmed, not at the grant that fits alone: {b:?}");
    assert_eq!(b.1, regions_of(b.0, 3, 16));
    assert!(held <= 600 * MIB && a.reserved + b.1 <= 600 * MIB, "held {held}: the capacity bounds both reservations");
    drop(a);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn unequal_peaks_draw_their_footprint_and_admission_reserves_it() {
    let program = Arc::new(p_group_peaks(64));
    assert_eq!((program.slots.max_live, (0..4).map(|s| program.slots.peak[s]).collect::<Vec<_>>()), (3, vec![1, 3, 3, 3]));
    let want = Grant::of(96 * MIB);
    // S2, the peak of the live limits: slot 0 at the whole grant is freed before slots 1-3 open at a third each; W4 workers a quarter
    assert_eq!(footprint(want, &program, 1), 96 * MIB + ARENA_ALLOWANCE, "the grant serially, and the leader arena's allowance");
    assert_eq!(footprint(want, &program, 4), 192 * MIB + ARENA_ALLOWANCE + 4 * 3 * MIB, "twice the grant at W4, and each worker's arena (S4: 96 MiB / 32 = its 3 MiB floor)");
    assert_eq!(regions_of(want, 3, 4), 128 * MIB, "D192: the max_live reservation is 4/3 of the grant");
    let chain = p_group_chain(64);
    assert_eq!((footprint(Grant::of(64 * MIB), &chain, 1), footprint(Grant::of(64 * MIB), &chain, 4)), (64 * MIB + ARENA_ALLOWANCE, regions_of(Grant::of(64 * MIB), 2, 4) + ARENA_ALLOWANCE + 4 * worker_arena(Grant::of(64 * MIB), 4)), "equal peaks: two live halves serially, regions_of at W4");
    let _g = armed(1 << 30);
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let a = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, 1), Duration::from_millis(10)).unwrap());
    assert_eq!(a.reserved, 96 * MIB + ARENA_ALLOWANCE);
    let refused = POOL.stats().refusals;
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted().len(), 8, "eight groups of v % 8");
    // the draws are cumulative (slot 0's region freed, then three more): G's room held the live peak, no draw refused
    assert!(a.drawn() > a.reserved, "drawn {} over a live peak of {}", a.drawn(), a.reserved);
    assert_eq!(POOL.stats().refusals, refused, "every draw fit the peak reservation");
    drop(a);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D193: a GROUP BY whose merge has an owner reserves the one-owner fold's pinned region beside its regions — at W4 over the
/// workers' shares, serially over the slot's table — so a reservation of the whole pool leaves the fold room (it drew 4 MiB
/// past a pool-exact reservation: ERROR out of memory where PG answers).
#[test]
fn an_owner_merges_pinned_region_is_in_the_footprint() {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ORDER_NONE};
    let plain = p_group_share(64);
    let d = gravity_program::OwnerDesc::generic(RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]), ORDER_NONE, 0);
    let owned = p_group_share_owned(64, d);
    let want = Grant::of(512 * MIB);
    let w4 = want.split(1, 4).worker.bytes as u64 * 4;
    assert_eq!(footprint(want, &owned, 4), footprint(want, &plain, 4), "W4: a one-owner fold cuts its region in the idle leader region (or its round-2 floor there)");
    let _ = w4;
    assert_eq!(footprint(want, &owned, 1), footprint(want, &plain, 1) + (4 * MIB + 2 * 512 * MIB), "serially: the slot's table");
}

/// D2845: a kernel whose arena grow past W and G the full pool refuses returns its scratch refusal (kind 2); the statement's
/// error is that refusal by name, 53200 out of memory, as a refused breaker region's is (a 2 M-group GROUP BY alone in a
/// 256 MiB pool: its owner grew 128 MB past a 64 MiB grant and the emit's 16 MB gather array raised "unsupported execution
/// (kind 2)"). A scratch refusal in a statement the pool refused nothing stays kind 2.
#[test]
fn a_scratch_refusal_after_the_pool_refused_an_arena_grow_is_the_named_out_of_memory() {
    use gravity_abi::status::{unsupported, DONE, UNSUPPORTED_SCRATCH};
    use gravity_program::Code;
    unsafe extern "C-unwind" fn k_grow(ctx: *mut gravity_abi::Ctx) -> i64 {
        // SAFETY: the driver binds `ctx` for the call
        if unsafe { gravity_abi::program::arena_grow(ctx, 64 << 20) } == 0 { unsupported(UNSUPPORTED_SCRATCH) } else { DONE }
    }
    unsafe extern "C-unwind" fn k_scratch(_ctx: *mut gravity_abi::Ctx) -> i64 {
        unsupported(UNSUPPORTED_SCRATCH)
    }
    let _g = armed(16 * MIB);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let mut entry = Entry::new(2);
    for (k, named) in [(k_grow as unsafe extern "C-unwind" fn(*mut gravity_abi::Ctx) -> i64, true), (k_scratch, false)] {
        let program = Arc::new(p_code(Code::hand(k), 1, 0));
        let a = std::rc::Rc::new(POOL.admit(Grant::of(MIB), 1, 1, Duration::from_millis(10)).unwrap());
        let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
        let r = entry.run(&mut exec, &mut b, gravity_driver::Call::batch(1));
        if named {
            assert_eq!(a.refused(), 64 * MIB, "the refused grow is kept on the statement");
            assert!(matches!(r, Err(Error::OutOfMemory { bytes, capacity, .. }) if bytes == 64 * MIB && capacity == 16 * MIB), "{r:?}");
        } else {
            assert_eq!((a.refused(), r), (0, Err(Error::Unsupported(UNSUPPORTED_SCRATCH))), "no refused draw: the kernel's own kind 2");
        }
        entry.end(exec, false);
        drop(a);
    }
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// DR-12 (D285, D2845): a parallel owner whose records (the breaker cost's held estimate, D365) outgrow the idle leader
/// region reserves the one owner's region the overflow pins past G (`one_owner_bytes` over them); one whose records fit
/// the leader region reserves nothing past it.
#[test]
fn a_parallel_owner_past_the_leader_region_reserves_its_records_bound() {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ORDER_NONE};
    let d = gravity_program::OwnerDesc::generic(RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]), ORDER_NONE, 0);
    let want = Grant::of(64 * MIB);
    let (few, few_owned) = (p_group_spill_rows(64, 1000, 3, None), p_group_spill_rows(64, 1000, 3, Some(d)));
    let (many, many_owned) = (p_group_spill_rows(64, 20_000_000, 10, None), p_group_spill_rows(64, 20_000_000, 10, Some(d)));
    let cost = gravity_program::cost::breaker::of(&many_owned.pipelines[0]).expect("a share-budgeted breaker");
    assert!(cost.held > 64 * MIB, "the estimate's records ({} B) outgrow a 64 MiB grant's leader region", cost.held);
    assert_eq!(footprint(want, &few_owned, 8), footprint(want, &few, 8), "W8, records that fit the leader region: nothing past it");
    let bound = gravity_driver::one_owner_bytes(cost.held as usize, cost.record as usize).min(u32::MAX as usize & !7) as u64;
    assert_eq!(footprint(want, &many_owned, 8) - footprint(want, &many, 8), bound, "W8: the one owner's region over the held records");
}

/// D2845: a serial owner (W0, or W1) pins its fold beside the slot it reads at least at its records' bound (`own_all` lays
/// `one_owner_bytes` over the records the breaker cost holds, D365), at most the wall; one whose records are few keeps the
/// 4 MiB beside twice the slot's bytes (DR-11).
#[test]
fn a_serial_owner_pins_at_least_its_records_bound() {
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ORDER_NONE};
    let d = gravity_program::OwnerDesc::generic(RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]), ORDER_NONE, 0);
    let want = Grant::of(64 * MIB);
    let (few, few_owned) = (p_group_spill_rows(64, 1000, 3, None), p_group_spill_rows(64, 1000, 3, Some(d)));
    let (many, many_owned) = (p_group_spill_rows(64, 20_000_000, 10, None), p_group_spill_rows(64, 20_000_000, 10, Some(d)));
    let cost = gravity_program::cost::breaker::of(&many_owned.pipelines[0]).expect("a share-budgeted breaker");
    let bound = gravity_driver::one_owner_bytes(cost.held as usize, cost.record as usize).min(u32::MAX as usize & !7) as u64;
    for width in [0u16, 1] {
        let pin = footprint(want, &few_owned, width) - footprint(want, &few, width);
        assert!(pin >= 4 * MIB, "W{width}: a few records' owner keeps the 4 MiB beside twice the slot ({pin} B)");
        assert!(bound > pin, "W{width}: the estimate's records' bound ({bound} B) outgrows the slot's pin ({pin} B)");
        assert_eq!(footprint(want, &many_owned, width) - footprint(want, &many, width), bound, "W{width}: the records' bound");
    }
}
