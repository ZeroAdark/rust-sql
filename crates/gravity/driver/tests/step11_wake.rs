//! Ladder step 1.1 (`PGRUST_MEMPOLICY=gate_now=1`), the wake: a draw past W and G waiting for room parks on the free
//! epoch (`MemWait::park_free`, the grant pool's park table), and another statement's end wakes it: no 2 ms poll. One test:
//! it installs the process's spill factory, arms the process-wide pool and sets the process's knobs.

use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, install_spill_factory, Entry, Grant, SpillFactory, SpillSet, TempMap, POOL, POOL_FRACTION_WHOLE, REGIONS};
use mempolicy::consts::{set, K};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// A session's spill sets as the server's: the rig's sets, and past the pool a temp file mapped shared, unlinked once
/// mapped (`map_temp`).
struct FileRoad;

impl SpillFactory for FileRoad {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn map_temp(&self, bytes: usize) -> gravity_driver::Result<Option<TempMap>> {
        use std::os::fd::AsRawFd;
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("step11w-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).expect("a temp file");
        f.set_len(bytes as u64).expect("its length");
        let m = TempMap::shared(f.as_raw_fd(), bytes);
        std::fs::remove_file(&path).expect("unlinked once mapped");
        Ok(m)
    }

    fn maps_temp(&self) -> bool {
        true
    }
}

static FILE_ROAD: FileRoad = FileRoad;

/// The memory waits that paused (`set_mem_wait`'s start: once per wait, at its first pause).
static PAUSED: AtomicU64 = AtomicU64::new(0);

fn paused() {
    PAUSED.fetch_add(1, Relaxed);
}

fn resumed() {}

#[test]
fn a_statements_end_wakes_a_parked_draw() {
    assert!(install_spill_factory(&FILE_ROAD), "this binary owns the process's spill factory");
    set(K::GateNow, 1);
    // a long wait for room, so the free below (not the 200 ms refusal) ends it
    set(K::GateRefuseMs, 20_000);
    gravity_driver::mem::grant::set_mem_wait(paused, resumed);
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, 1);
    let spare = 16 * MIB;
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(2 * need + spare);
    let res = Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, 1), Duration::from_millis(10)).expect("admitted"));
    res.set_pool_fraction(POOL_FRACTION_WHOLE);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");
    let ctx = exec.node().ctx();
    let carved = res.hold(res.left()).expect("G's room held");

    // another session's statement holds the pool but `spare` and ends 300 ms after the draw below began
    let (admitted, go) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, g) = (admitted.clone(), go.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        a.store(true, Relaxed);
        let t0 = Instant::now();
        while !g.load(Relaxed) && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(300));
        let ended = Instant::now();
        drop(r);
        ended
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let ask = spare + MIB;
    let (parks, pauses) = (gravity_driver::mem::grant::PARKS.load(Relaxed), PAUSED.load(Relaxed));
    go.store(true, Relaxed);
    let t1 = Instant::now();
    // SAFETY: the leader's context, bound for the statement, on this thread
    let q = unsafe { gravity_driver::hostimpl::arena::spill_region(ctx, ask as usize) };
    let (took, served_at) = (t1.elapsed(), Instant::now());
    let ended = other.join().expect("the other statement");
    let parked = gravity_driver::mem::grant::PARKS.load(Relaxed) - parks;
    set(K::GateNow, 0);
    set(K::GateRefuseMs, 200);

    assert!(!q.is_null(), "served once the other statement ended ({took:?})");
    assert_eq!(PAUSED.load(Relaxed), pauses + 1, "one memory wait");
    assert!(parked >= 1, "the wait parked on the free epoch ({parked} parks)");
    // 300 ms with a 50 ms net: a wait that only polled the net would park ~6 times; woken by the free, it parks no more
    // than that and is served within the net of the end
    assert!(served_at.saturating_duration_since(ended) < Duration::from_millis(45), "served within the net of the free ({:?} after)", served_at.saturating_duration_since(ended));
    assert!(took < Duration::from_secs(5), "{took:?}");
    // SAFETY: `q` is a region of this statement's arena (`spill_region`), held until the statement ends
    let _ = q;

    drop(carved);
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
