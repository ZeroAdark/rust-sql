//! Ladder step 1.1 (`PGRUST_MEMPOLICY=gate_now=1`): the grant pool's gate answers at once. A draw past W and G that
//! misses the pool waits only for bytes already on their way; with none, a draw with a road past the pool (a session's
//! mapped temp file, `spill_host::v4::take`) takes it at once, and one without (`arena::spill_region`: its refusal is the
//! statement's) waits `gate_refuse_ms` (200 ms) for room, then refuses. Trunk's draw waited the pool's 2 s budget while
//! another statement held the pool, whether or not it would give anything back (the D4439 crawl, D4425's 2 s reloads).
//! One test: it installs the process's spill factory, arms the process-wide pool and sets the process's knobs.

use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::mem::spill_host::v4;
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
        let path = std::env::temp_dir().join(format!("step11-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
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
fn a_pool_miss_waits_only_for_bytes_on_their_way() {
    assert!(install_spill_factory(&FILE_ROAD), "this binary owns the process's spill factory");
    set(K::GateNow, 1);
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

    // G held whole: every draw below is past W and G
    let carved = res.hold(res.left()).expect("G's room held");
    // another session's statement holds the pool but `spare`, and gives nothing back until told to
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        a.store(true, Relaxed);
        let t0 = Instant::now();
        while !d.load(Relaxed) && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(r);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let ask = spare + MIB;
    assert!(POOL.capacity() - POOL.held() < ask, "past the pool's room: {} B free", POOL.capacity() - POOL.held());

    // (1) a draw with a road past the pool (the session's temp file): the file at once, no memory wait
    let stats_0 = mcx::ladder_stats::site(mcx::ladder_stats::Site::Draw);
    let (held, pauses, mapped) = (POOL.held(), PAUSED.load(Relaxed), v4::SPILL_MAPPED.load(Relaxed));
    let t0 = Instant::now();
    // SAFETY: the leader's context, bound for the statement, on this thread
    let p = unsafe { v4::take(ctx, ask as usize) };
    let took = t0.elapsed();
    let (held_1, pauses_1, mapped_1) = (POOL.held(), PAUSED.load(Relaxed), v4::SPILL_MAPPED.load(Relaxed));
    // SAFETY: `p` is the owner's region until its `give`
    if !p.is_null() {
        unsafe { v4::give(ctx, p) };
    }

    // (2) a draw with no road past the pool (its refusal is the statement's): 200 ms for room, then refused
    let stats_1 = mcx::ladder_stats::site(mcx::ladder_stats::Site::Draw);
    let pauses_2 = PAUSED.load(Relaxed);
    let t1 = Instant::now();
    // SAFETY: as above
    let q = unsafe { gravity_driver::hostimpl::arena::spill_region(ctx, ask as usize) };
    let waited = t1.elapsed();
    let pauses_3 = PAUSED.load(Relaxed);
    let stats_2 = mcx::ladder_stats::site(mcx::ladder_stats::Site::Draw);

    done.store(true, Relaxed);
    other.join().expect("the other statement");
    set(K::GateNow, 0);

    assert!(!p.is_null(), "(1) served ({took:?})");
    assert_eq!(mapped_1, mapped + 1, "(1) by the statement's mapped temp file ({took:?})");
    assert_eq!(held_1, held, "(1) nothing drawn from the pool");
    assert_eq!(pauses_1, pauses, "(1) no memory wait for room another statement holds and gives nothing back ({took:?})");
    assert!(q.is_null(), "(2) refused: the pool stayed short ({waited:?})");
    assert_eq!(pauses_3, pauses_2 + 1, "(2) it waited for room ({waited:?})");
    assert!(waited >= Duration::from_millis(190) && waited < Duration::from_millis(1500), "(2) ~200 ms for room, then refused, never the 2 s budget ({waited:?})");
    // pgrust_memory_ladder_stats() counts a gate_now wait at its gate's site, as it counts take_wait's
    assert!(stats_1.waits > stats_0.waits, "(1) ladder_stats counts the miss's wait at the draw site ({stats_0:?} -> {stats_1:?})");
    assert_eq!(stats_2.waits, stats_1.waits + 1, "(2) ladder_stats counts its wait at the draw site ({stats_1:?} -> {stats_2:?})");
    assert_eq!(stats_2.refusals, stats_1.refusals + 1, "(2) ladder_stats counts its refusal ({stats_1:?} -> {stats_2:?})");
    assert!(stats_2.wait_ns - stats_1.wait_ns >= 190_000_000, "(2) ladder_stats counts its ~200 ms ({stats_1:?} -> {stats_2:?})");

    drop(carved);
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
