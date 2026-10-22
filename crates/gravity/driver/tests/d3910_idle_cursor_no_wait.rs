//! D3910 (a): a draw past W and G never waits on a grant holder whose portal sits suspended between fetches (an idle cursor:
//! no fetch may ever come, and PostgreSQL never makes a session wait on another's idle cursor). Before, a GROUP BY beside
//! another session's idle cursor holding 26 MiB of an 80 MiB pool waited the pool's whole 2 s wait at its owner phase, past
//! its statement_timeout. Here the cursor has drawn half its grant (D3910 (b) lends back only the rest), so only its end
//! could give those bytes back. One test per process (the pool is process-wide).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

#[test]
fn a_draw_past_g_never_waits_on_an_idle_cursor() {
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
    let idle0 = mcx::governor::idle_holders();

    // another session's statement holds the rest of the pool but `spare`, draws half of it, and its portal suspends (a
    // cursor between fetches)
    let (parked, release) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (p, rel) = (parked.clone(), release.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the cursor's statement");
        let drawn = REGIONS.take((need / 2) as usize, Some(&r)).expect("the cursor's rows");
        r.park_slot();
        p.store(true, Relaxed);
        while !rel.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
        REGIONS.put(drawn);
        drop(r);
    });
    let t0 = Instant::now();
    while !parked.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the cursor's statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(mcx::governor::idle_holders(), idle0 + 1, "the suspended portal is an idle holder");

    // a chunk only the cursor's drawn bytes could serve: refused at once, not after the pool's 2 s wait
    let past = res.left() + spare + need / 2 + MIB;
    let (refusals, held) = (POOL.stats().refusals, POOL.held());
    let t0 = Instant::now();
    assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, past) }, 0, "refused: no room, and an idle cursor gives none back");
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(100), "no wait on the idle cursor ({took:?})");
    assert_eq!((POOL.held(), POOL.stats().refusals), (held, refusals + 1), "the refusal took nothing and is counted");

    release.store(true, Relaxed);
    other.join().expect("the cursor's statement");
    assert_eq!(mcx::governor::idle_holders(), idle0, "the cursor's close ends its idle hold");
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every chunk and reservation back at the statement's end");
    POOL.arm(0);
}
