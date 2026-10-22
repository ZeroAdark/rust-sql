//! `PGRUST_POOL_RESERVE` on: the claimer's target is min(1.1 x the deepest drawdown, at least the floor; the budget),
//! a miss raises it at once, a free WarmDirty superpage is zeroed in place before any Cold one is populated, the spare
//! past the target ages out in pressure-only mode, and the ladder's shed takes the reserve to 0, under the floor.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const FLOOR: usize = 4 * MIB;
const MAX: usize = 16 * MIB;

fn wait_for(what: &str, f: impl Fn(&Stats) -> bool) -> Stats {
    let t0 = Instant::now();
    loop {
        let s = stats();
        if f(&s) {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "{what}: {s:?} {:?}", reserve_stats());
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn met(target: usize) {
    wait_for("the claimer met its target", |s| s.warm_target == target && s.warm_zero >= target && hooks::claimer_met());
    std::thread::sleep(Duration::from_millis(50));
}

/// The reserve's own account (memledger Acct::POOL_RESERVE).
fn account() -> i64 {
    memledger::account_row(memledger::Acct::POOL_RESERVE).account_bytes
}

/// memacct 9d (22:04:33Z) condition 1's witness: the account is never below zero, at every step (not only at quiescence).
fn step(what: &str) {
    let a = account();
    assert!(a >= 0, "{what}: pool_reserve account {a} < 0: {:?} {:?}", stats(), reserve_stats());
}

fn want_of(drawdown: usize) -> usize {
    (drawdown + drawdown / 10).max(FLOOR).next_multiple_of(SUPER)
}

#[test]
fn on_sizes_the_reserve_from_drawdowns() {
    hooks::reserve_switch(true);
    init_with(|c| {
        c.warm_zero = FLOOR;
        c.warm_zero_max = MAX;
        c.idle_drain_ms = 600_000;
    });
    assert!(start());
    met(FLOOR);
    assert!(reserve_stats().on);
    step("met the floor");
    // and between the steps: a reader the whole test long keeps the lowest account it saw
    static LOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(i64::MAX);
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let watcher = std::thread::spawn(|| {
        while !DONE.load(std::sync::atomic::Ordering::Relaxed) {
            LOW.fetch_min(account(), std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(Duration::from_micros(200));
        }
    });

    // a miss raises the target at once: 1.1 x (the floor drawn + two superpages missed)
    hooks::claimer_hold(true);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let mut held: Vec<Span> = (0..FLOOR / SUPER + 2).map(|_| span(SUPER, h).expect("span")).collect();
    step("drawn and missed");
    let r = reserve_stats();
    assert_eq!(r.drawdown, FLOOR + 2 * SUPER, "{r:?}");
    let t1 = want_of(FLOOR + 2 * SUPER);
    assert_eq!(stats().warm_target, t1, "{r:?}");
    hooks::claimer_hold(false);
    met(t1);
    step("refilled to t1");
    let p0 = stats().prefaulted;

    // recycle in place: the held superpages come back dirty; a refill zeroes them, populating nothing new
    for s in held.drain(..) {
        // SAFETY: a span we hold, written then given back whole.
        unsafe { core::ptr::write_bytes(s.as_ptr(), 0xab, s.len()) };
        unsafe { give_span(s) };
    }
    step("given back dirty");
    let dirty = stats().warm_dirty;
    assert_eq!(dirty, FLOOR + 2 * SUPER);
    hooks::claimer_hold(true);
    let dense = Hint::new(Class::Task, Tag::OTHER).zero(Zero::Dense);
    let k = t1 / SUPER;
    held.extend((0..k).map(|_| span(SUPER, dense).expect("span")));
    let r0 = reserve_stats();
    assert_eq!(r0.miss_bytes, r.miss_bytes, "a dense take found warm zero: no miss");
    for s in held.drain(..) {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    hooks::claimer_hold(false);
    let s = wait_for("the claimer refilled", |s| s.warm_zero >= s.warm_target && hooks::claimer_met());
    step("recycled in place");
    let r1 = reserve_stats();
    assert!(r1.recycled > r0.recycled, "dirty superpages zeroed in place: {r1:?}");
    assert_eq!(s.prefaulted, p0, "no Cold superpage populated while dirty ones were free: {s:?} {r1:?}");
    hooks::claimer_hold(true);
    let all: Vec<Span> = (0..s.warm_zero / SUPER).map(|_| span(SUPER, dense).expect("span")).collect();
    for sp in &all {
        // SAFETY: a span we hold, read only.
        let b = unsafe { core::slice::from_raw_parts(sp.as_ptr(), sp.len()) };
        assert!(b.iter().all(|&x| x == 0), "a recycled superpage reads zero");
    }
    for sp in all {
        // SAFETY: ours, not used again.
        unsafe { give_span(sp) };
    }

    // the spare past the budget ages out (pressure-only mode): one superpage a remap (the claimer held: this thread's pass)
    set_reserve_budget(FLOOR as u64);
    std::thread::sleep(Duration::from_millis(1_100));
    hooks::clear();
    // a pass gives back at most 16 MiB or 2 ms of remaps; the rest waits a pass
    for _ in 0..50 {
        hooks::reserve_pass(true);
        step("a spare release pass");
        let s = stats();
        if s.warm_zero + s.warm_dirty <= FLOOR {
            break;
        }
    }
    let s = stats();
    let r = reserve_stats();
    assert_eq!(s.warm_target, FLOOR, "min(want, the ladder's budget): {r:?}");
    assert!(r.spare_released > 0 && s.warm_zero + s.warm_dirty <= FLOOR.max(s.warm_target), "the spare went back: {s:?} {r:?}");
    let mut gives = 0;
    let whole = hooks::each(|op, at, len, advice| {
        assert!(!(op == hooks::Op::Madvise && advice == 4 && len >= SUPER), "CVE-2026-74674: a MADV_DONTNEED over {len} at {at:#x}");
        if op == hooks::Op::GiveBack {
            assert_eq!((at % SUPER, len), (0, SUPER), "one whole superpage a remap");
            gives += 1;
        }
    });
    assert!(whole && gives > 0, "the spare's releases logged: {gives}");
    hooks::claimer_hold(false);

    // the reserve's own account holds its free warm superpages, posted as they come and go (the claimer held: quiet)
    wait_for("refilled to the budget", |s| s.warm_zero >= FLOOR && hooks::claimer_met());
    hooks::claimer_hold(true);
    std::thread::sleep(Duration::from_millis(20));
    let s = stats();
    assert_eq!(account(), (s.warm_zero + s.warm_dirty) as i64, "{s:?}");

    // a pressure purge (an inline caller's purge_now among them) takes the spare and the garbage, never the reserve
    // once the ladder sizes it: that goes only through the ladder's shed
    let held = s.warm_zero + s.warm_dirty;
    purge_now(usize::MAX / 2, None);
    step("a pressure purge");
    let s = stats();
    assert_eq!(s.warm_zero + s.warm_dirty, held.min(FLOOR), "the reserve's bytes stay held: {s:?}");
    assert_eq!(account(), (s.warm_zero + s.warm_dirty) as i64);

    // the ladder's background pass sheds the reserve to 0, under the floor (asked of the purger), and nothing refills it
    // until the budget is back (the claimer held across the ask, so no fill is in flight)
    let asked = reserve_ask(1);
    assert_eq!(asked, SUPER, "one byte asked: one whole superpage");
    wait_for("the purger served the first shed", |_| reserve_stats().shed_runs >= 1);
    step("the first shed served");
    assert_eq!(stats().warm_target, FLOOR - SUPER, "the budget comes down to what is left: {:?}", reserve_stats());
    let asked = reserve_ask(usize::MAX);
    assert!(asked > 0 && stats().warm_target == 0, "the rest: ladder-sized (budget set above): {:?}", reserve_stats());
    wait_for("the purger served the shed", |_| reserve_stats().shed_runs >= 2);
    step("shed to 0");
    hooks::claimer_hold(false);
    let s = stats();
    let r = reserve_stats();
    assert_eq!((s.warm_zero, s.warm_dirty, s.warm_target, account()), (0, 0, 0, 0), "{r:?}");
    assert!(r.shed as usize >= asked + SUPER && r.shed_clock_ns > 0 && r.remap_max_ns > 0, "{r:?}");
    std::thread::sleep(Duration::from_millis(1_100));
    assert_eq!(stats().warm_zero, 0, "budget 0: no refill");
    set_reserve_budget(u64::MAX);
    wait_for("the bound is back: refilled", |s| s.warm_target >= FLOOR && s.warm_zero >= s.warm_target);
    step("refilled after the shed");
    hooks::claimer_hold(true);
    std::thread::sleep(Duration::from_millis(20));
    let s = stats();
    assert_eq!(account(), (s.warm_zero + s.warm_dirty) as i64, "{s:?}");
    hooks::claimer_hold(false);
    DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher.join().expect("watcher");
    let low = LOW.load(std::sync::atomic::Ordering::Relaxed);
    assert!(low >= 0, "the pool_reserve account went to {low} between the steps");
    stop();
    balanced();
}
