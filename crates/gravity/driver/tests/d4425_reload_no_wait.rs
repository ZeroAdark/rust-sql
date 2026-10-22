//! D4425: a spilled hash join's reload regions (lowerv4 hashjoin spill/raw/load.rs: a partition's reload buffer, its kept
//! rows' offsets, the pair's laid-out region; spill/raw.rs: the split's drain records; `v4::take_reload`) past W and G, in a
//! session with a temp-file road, take the pool's room as it is now and then the statement's mapped temp file, with no
//! wait. On `take`'s road each one waited out the pool's draw budget (2 s, `GrantPool::take_wait`) while another statement
//! held the pool, and only then mapped the file: four trimmed W4 joins in cell e2 (8G) ran 423-446 s, 85% of their
//! workers' off-CPU time in that wait. One test: it installs the process's spill factory and arms the process-wide pool.

use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{footprint, install_spill_factory, Entry, Grant, SpillFactory, SpillSet, TempMap, POOL, POOL_FRACTION_WHOLE, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// A session's spill sets as the server's (pool `SpillSets`): the rig's sets, and past the pool a temp file mapped shared,
/// unlinked once mapped (`map_temp`).
struct FileRoad;

impl SpillFactory for FileRoad {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn map_temp(&self, bytes: usize) -> gravity_driver::Result<Option<TempMap>> {
        use std::os::fd::AsRawFd;
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("d4425-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
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

/// The memory waits that paused (`set_mem_wait`'s start: once per wait, at its first pause): waited or not is read by the
/// pauses, not by wall-clock bounds that follow the box's load.
static PAUSED: AtomicU64 = AtomicU64::new(0);

fn paused() {
    PAUSED.fetch_add(1, Relaxed);
}

fn resumed() {}

#[test]
fn a_reload_region_past_the_pool_maps_its_temp_file_with_no_wait() {
    assert!(install_spill_factory(&FILE_ROAD), "this binary owns the process's spill factory");
    assert!(gravity_driver::mem::spill::file_road(), "the factory maps temp files");
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

    // G held whole: the reload below is past W and G
    let carved = res.hold(res.left()).expect("G's room held");
    assert_eq!(res.left(), 0);
    // another session's statement holds the pool but `spare` until the reload below has its region: a holder whose end
    // could give room back, so `take`'s road waits its budget for it
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        a.store(true, Relaxed);
        let t0 = Instant::now();
        while !d.load(Relaxed) && t0.elapsed() < Duration::from_secs(30) {
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
    let (held, pauses, mapped) = (POOL.held(), PAUSED.load(Relaxed), v4::SPILL_MAPPED.load(Relaxed));
    let t0 = Instant::now();
    // SAFETY: the leader's context, bound for the statement, on this thread
    let p = unsafe { v4::take_reload(ctx, ask as usize) };
    let took = t0.elapsed();
    let held_after = POOL.held();
    done.store(true, Relaxed);
    other.join().expect("the other statement");

    assert!(!p.is_null(), "the reload region is served ({took:?})");
    assert_eq!(v4::SPILL_MAPPED.load(Relaxed), mapped + 1, "by the statement's mapped temp file ({took:?})");
    assert_eq!(held_after, held, "nothing drawn from the pool");
    assert_eq!(PAUSED.load(Relaxed), pauses, "no memory wait for the room another statement holds: the file at once ({took:?})");
    // the region is zeroed memory the reload writes and reads
    // SAFETY: `p` is `ask` bytes this owner holds until its `give`
    unsafe {
        assert_eq!((*p, *p.add(ask as usize - 1)), (0, 0), "zeroed");
        p.write_bytes(0x5a, ask as usize);
        assert_eq!(*p.add(ask as usize - 1), 0x5a);
        v4::give(ctx, p);
    }

    drop(carved);
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
