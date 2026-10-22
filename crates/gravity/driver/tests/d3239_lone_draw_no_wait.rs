//! D3239: a draw past W and G that the pool refuses waits (`BYTES_WAIT`, 2 s) only while another admitted statement is in
//! flight, whose end could give room back. A lone statement's draw is refused at once: TPC-DS q39 at SF10 W0, alone, waited
//! the whole 2 s at every hosted arena chunk past its halved grant (`m6_agg_final` -> arena grow -> `road_region` ->
//! `take_wait`), for minutes. Here, a leader arena chunk past G (the hosted aggregate's road): with a second statement in
//! flight it waits and takes the room that statement's end frees; alone it is refused at once. One test per process (the
//! pool is process-wide).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// D3165's sibling: the memory waits that paused (`set_mem_wait`'s start: once per wait, at its first pause). Waited or
/// refused at once is read by the pauses, not by wall-clock bounds that follow the box's load.
static PAUSED: AtomicU64 = AtomicU64::new(0);

fn paused() {
    PAUSED.fetch_add(1, Relaxed);
}

fn resumed() {}

#[test]
fn a_draw_past_g_waits_only_while_another_statement_could_give_room_back() {
    gravity_driver::mem::grant::set_mem_wait(paused, resumed);
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

    // G drawn down to a MiB or less (a chunk inside G never meets the pool's wait), so the chunk past G below stays within
    // the floor's slack past the reservation (exactsize D3226) beside a second admitted statement
    let fill = res.left().saturating_sub(MIB);
    if fill > 0 {
        assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, fill) }, fill as i64, "a chunk inside G");
    }
    // another session's statement holds the rest of the pool but `spare`, and ends once the draw below waits for it (a
    // pause; a fixed 300 ms ended it before the draw began on a loaded box)
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        let pauses = PAUSED.load(Relaxed);
        a.store(true, Relaxed);
        let t0 = Instant::now();
        while PAUSED.load(Relaxed) == pauses {
            assert!(t0.elapsed() < Duration::from_secs(20), "the draw never waited");
            std::thread::sleep(Duration::from_millis(1));
        }
        d.store(true, Relaxed);
        drop(r);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let past = res.left() + spare + MIB;
    assert!(past <= need, "the chunk fits the room the other statement's end frees: {past} B, {need} B");
    let (held, pauses) = (POOL.held(), PAUSED.load(Relaxed));
    let t0 = Instant::now();
    let got = unsafe { gravity_abi::program::arena_grow(ctx, past) };
    let took = t0.elapsed();
    assert!(done.load(Relaxed), "the chunk came only after the other statement ended ({took:?})");
    assert_eq!(got, past as i64, "with another statement in flight the draw waits and takes the room its end frees");
    assert!(PAUSED.load(Relaxed) > pauses, "waited for that end, in a memory wait ({took:?})");
    other.join().expect("the other statement");
    assert!(POOL.held() >= past && POOL.held() < held + past, "the other's bytes back, the chunk's taken: held {} of {held} before", POOL.held());

    // alone now: a chunk past the capacity can only come from this statement's own bytes, so no wait
    REGIONS.trim();
    let (refusals, held) = (POOL.stats().refusals, POOL.held());
    let over = res.left() + POOL.capacity() - held + MIB;
    let pauses = PAUSED.load(Relaxed);
    assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, over) }, 0, "past the capacity: refused");
    assert_eq!(PAUSED.load(Relaxed), pauses, "alone: refused at once, with no pause for the pool's 2 s wait");
    assert_eq!((POOL.held(), POOL.stats().refusals), (held, refusals + 1), "the refusal took nothing and is counted");

    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every chunk and reservation back at the statement's end");
    POOL.arm(0);
}
