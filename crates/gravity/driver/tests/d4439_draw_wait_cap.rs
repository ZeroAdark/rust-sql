//! D4439 (`PGRUST_MEMPOLICY=draw_wait_cap`, with pgrust.memory_ladder = on): with the ladder on and gate_now 0 a draw
//! past W and G the pool has no room for waited its budget whenever another statement held the pool, whether or not that
//! statement would give anything back, and a statement's draws one after another each waited again (S1 s1hagg: 372
//! waits, 2544 s, every hash aggregate at its 600 s timeout). With the cap a statement's draw-gate waits stop at
//! `admit::DRAW_WAIT_CAP_X` budgets in all: its later draws are refused at once (their road), and the next statement has
//! its own cap. Born red with the knob off: every draw waits. One test per process (the pool is process-wide).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};
use mempolicy::consts::{set, K};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// The memory waits that paused (`set_mem_wait`'s start: once per wait, at its first pause).
static PAUSED: AtomicU64 = AtomicU64::new(0);

fn paused() {
    PAUSED.fetch_add(1, Relaxed);
}

fn resumed() {}

#[test]
fn a_statements_draw_waits_stop_at_the_cap_and_its_later_draws_take_their_road_at_once() {
    gravity_driver::mem::grant::set_mem_wait(paused, resumed);
    // a 25 ms budget: with no free a draw waits one budget, a statement four in all
    let budget = Duration::from_millis(25);
    set(K::BytesWaitMs, budget.as_millis() as u64);
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, 1);
    let spare = 16 * MIB;
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(2 * need + spare);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, 1), Duration::from_millis(10)).unwrap());
    res.set_pool_fraction(gravity_driver::POOL_FRACTION_WHOLE);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
    let ctx = exec.node().ctx();
    let fill = res.left().saturating_sub(MIB);
    if fill > 0 {
        assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, fill) }, fill as i64, "a chunk inside G");
    }
    // another session's statement holds the rest of the pool and gives nothing back while the draws run
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        a.store(true, Relaxed);
        while !d.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(r);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    // a chunk past G a MiB more than the pool's room beside the other statement (within the statement's cap: it would
    // fit once the other gave its bytes back, so the draw waits for that)
    let over = res.left() + spare + MIB;
    assert!(over <= need, "the chunk fits the room the other statement's end would free: {over} B, {need} B");
    // a draw past the pool's room: whether it paused (waited) before its refusal
    let draw = || {
        let p = PAUSED.load(Relaxed);
        assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, over) }, 0, "past the pool's room: refused");
        PAUSED.load(Relaxed) > p
    };
    mcx::ladder::publish(mcx::ladder::ON, 1 << 40);
    mcx::governor::statement_start(0, true);

    // off: every draw waits, however long the statement has waited already
    set(K::DrawWaitCap, 0);
    let capped = gravity_driver::mem::grant::DRAW_CAPPED.load(Relaxed);
    for i in 0..6 {
        assert!(draw(), "off: draw {i} waited");
    }
    assert_eq!(gravity_driver::mem::grant::DRAW_CAPPED.load(Relaxed), capped, "off: none capped");
    // D4914: each refusal after its wait names its branch on the statement
    let after_wait = mcx::governor::stmt_refusal().and_then(|(w, _, _)| mcx::ladder_stats::Why::from_u8(w));
    assert!(matches!(after_wait, Some(mcx::ladder_stats::Why::WaitSpent | mcx::ladder_stats::Why::LastTry)), "off: refused after its wait ({after_wait:?})");
    let whys = |w: mcx::ladder_stats::Why| mcx::ladder_stats::whys()[mcx::ladder_stats::Why::ALL.iter().position(|&x| x == w).unwrap()].0;
    let capped_whys = whys(mcx::ladder_stats::Why::DrawCapped);

    // on, a new statement: its draws wait until their waits reach the cap, then each is refused at once
    set(K::DrawWaitCap, 1);
    mcx::governor::statement_start(0, true);
    let cap = mempolicy::admit::draw_wait_cap().expect("on: a cap");
    assert_eq!(cap, budget * mempolicy::admit::DRAW_WAIT_CAP_X, "four budgets in all");
    let waited: Vec<bool> = (0..8).map(|_| draw()).collect();
    let first = waited.iter().position(|w| !w).expect("on: a draw past the cap took its road at once");
    assert!(first >= 1 && waited[first..].iter().all(|w| !w), "on: the draws waited until the cap, none after it ({waited:?})");
    assert!(mcx::governor::stmt_draw_waited_ns().expect("the statement's count") >= cap.as_nanos() as u64, "the statement's waits reached the cap");
    assert_eq!(gravity_driver::mem::grant::DRAW_CAPPED.load(Relaxed) - capped, (waited.len() - first) as u64, "the draws past the cap, counted");
    // D4914: the cap's refusals by branch: the draws past it (refused at once, `DRAW_CAPPED`) and the draw whose wait it
    // ended, if one did (its wait reached the cap before its budget)
    let (by_branch, at_once) = (whys(mcx::ladder_stats::Why::DrawCapped) - capped_whys, (waited.len() - first) as u64);
    assert!(by_branch == at_once || by_branch == at_once + 1, "the cap's refusals by branch: {by_branch}, past it at once: {at_once}");
    let last = mcx::governor::stmt_refusal().expect("on: the statement's last refusal");
    assert_eq!(mcx::ladder_stats::Why::from_u8(last.0), Some(mcx::ladder_stats::Why::DrawCapped), "on: the last refusal is the cap's");
    assert!(last.1 > 0, "its bytes kept");

    // the next statement has its own cap
    mcx::governor::statement_start(0, true);
    assert_eq!(mcx::governor::stmt_draw_waited_ns(), Some(0), "the next statement: no wait counted yet");
    assert_eq!(mcx::governor::stmt_refusal(), None, "the next statement: no refusal kept yet");
    assert!(draw(), "the next statement's first draw waits");

    mcx::governor::statement_end();
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    done.store(true, Relaxed);
    other.join().expect("the other statement");
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(0);
}
