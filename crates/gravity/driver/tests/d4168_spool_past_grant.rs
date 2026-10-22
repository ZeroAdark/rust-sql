//! D4168 (C10, SPILL-DEBT's `SpoolPastGrant` paid): a spool's region (exec ops/material: a Material's, a CTE scan's, a
//! grouping-set node's record spool's) asked through `arena::object_region` in a process with a temp-file road
//! (`SpillFactory::maps_temp`, the session's spill sets) comes from W, else G; past them it is refused at once, with the
//! pool's held bytes unchanged and nothing counted on the uncapped road, and the spool's caller maps its temp file
//! (`object_map`), as PostgreSQL's tuplestore writes its rows out past work_mem. Before, it drew the pool past G for as
//! long as the pool had room, so a 64 MiB pool held a grouping-set spool to its last page and the statement's breaker
//! region failed out of memory. One test: it installs the process's spill factory and arms the process-wide pool.

use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_driver::hostimpl::arena::object_region;
use gravity_driver::mem::grow::uncapped_at;
use gravity_driver::{footprint, install_spill_factory, Entry, Grant, SpillFactory, SpillSet, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// A session's spill sets as the server's (pool `SpillSets`): a temp-file road (`maps_temp`); its sets the rig's.
struct FileRoad;

impl SpillFactory for FileRoad {
    fn create(&self) -> gravity_driver::Result<Box<dyn SpillSet>> {
        MEM_SPILL.create()
    }

    fn maps_temp(&self) -> bool {
        true
    }
}

static FILE_ROAD: FileRoad = FileRoad;

#[test]
fn a_spool_region_past_w_and_g_is_refused_at_once_for_its_temp_file_road() {
    assert!(install_spill_factory(&FILE_ROAD), "this binary owns the process's spill factory");
    assert!(gravity_driver::mem::spill::file_road(), "the factory maps temp files");
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    let program = Arc::new(p_count(4));
    let grant = Grant::of(4 * MIB);
    POOL.arm(footprint(grant, &program, 1) + 256 * MIB);
    let res = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, 1), Duration::from_millis(10)).expect("admitted"));
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");
    let ctx = exec.node().ctx();

    let at = uncapped_at();
    let small = 64 << 10;
    assert!(res.left() > small, "G has room for a small region ({} B left)", res.left());
    // SAFETY: the leader's context, bound for the statement, on this thread
    let within = unsafe { object_region(ctx, small as usize) }.expect("a spool region within G");

    let carved = res.hold(res.left()).expect("G's room held");
    assert_eq!(res.left(), 0);
    let held = POOL.held();
    assert!(POOL.capacity() - held > 8 * MIB, "the pool has room past G: the old road would have drawn it");
    // SAFETY: as above
    let past = unsafe { object_region(ctx, 8 * MIB as usize) };
    assert!(past.is_none(), "a spool region past W and G is refused at once: its caller maps its temp file");
    assert_eq!(POOL.held(), held, "the pool's room stays for the statement's breakers");
    assert_eq!(uncapped_at(), at, "nothing drawn past W and G, nothing counted on the uncapped road");

    drop((within, carved));
    entry.end(exec, false);
    drop(res);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
