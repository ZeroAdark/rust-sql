//! D254 (no runtime "past its share" errors before M7's spill.breakers): a breaker region that outgrows its share at run time
//! reopens larger (`region_grow` from the IR: the leader's slot at W1, a worker's partial at W >= 2; `State::grow` under the
//! merge, and in phase 2's layout) and the statement completes with every row. A build of 64x the share floor at W0 and at W8,
//! unadmitted and admitted (the grant far under the build: the grows draw past G, reserved from the pool), each against the
//! rows' exact totals. One test: the admitted half arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{footprint, Accounting, Call, Claimed, Entry, Grant, Merge, Partial, PartitionPlan, Region, Result, State, Step, POOL, REGIONS, SHARE_FLOOR};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;
/// 64x the 64 KiB share floor in records (16 bytes a row).
const BLOCKS: u64 = (64 * SHARE_FLOOR as u64 / 16) / PER_BLOCK;
const CLAIM: u64 = 16;

/// Phase 2 (partition-parallel) over the same regions: the whole fold happens in the layout step on a leader that grows
/// (`prepare_partitions_in`); the jobs check they see the grown destination.
struct GrowMergeP2 {
    grown: AtomicUsize,
}

impl Merge for GrowMergeP2 {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        grow_fold(leader, partial.bytes)
    }

    fn partition_plan(&self, _partials: &[Partial<'_>]) -> Option<PartitionPlan> {
        Some(PartitionPlan { pbits: 1, groups: 0, record: 16 })
    }

    fn prepare_partitions_in(&self, leader: &mut State<'_>, _plan: PartitionPlan, partials: &[Partial<'_>]) -> Result<()> {
        for p in partials {
            grow_fold(leader, p.bytes)?;
        }
        self.grown.store(leader.bytes.len(), Ordering::Release);
        Ok(())
    }

    fn merge_partition(&self, leader: Region, _part: u32, _plan: PartitionPlan, _partials: &[Partial<'_>]) -> Result<()> {
        assert_eq!(leader.len, self.grown.load(Ordering::Acquire), "phase 2's jobs see the destination the layout grew");
        Ok(())
    }
}

enum Arm {
    Serial,
    Merge,
    Phase2,
}

/// One run of the build at `width` (0 = the serial arm): the emitted totals and the Node's accounting.
fn run(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, res: std::rc::Rc<gravity_driver::Reservation>, width: u16, arm: Arm) -> (Vec<Vec<u64>>, Accounting) {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, derive, &mut b, res).unwrap();
    match arm {
        Arm::Serial => assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done),
        Arm::Merge | Arm::Phase2 => {
            let mut open = entry.open_section(&mut exec, &b, derive, 0, width).unwrap();
            let cursor = AtomicU64::new(0);
            'claims: loop {
                for o in 0..width {
                    let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
                    if lo >= BLOCKS {
                        break 'claims;
                    }
                    assert_eq!(open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS))).unwrap(), Claimed::Done);
                }
            }
            for o in 0..width {
                assert!(open.partial(o).bytes.len() > 2 * SHARE_FLOOR as usize, "worker {o}'s partial grew past twice its share");
            }
            if matches!(arm, Arm::Phase2) {
                let m = GrowMergeP2 { grown: AtomicUsize::new(0) };
                let plan = open.plan_partitions(&m).unwrap().expect("a plan");
                open.merge_partitions(&m, plan, 0, 2).unwrap();
                open.set_merged();
                assert!(open.leader_region().len() >= 64 * SHARE_FLOOR as usize, "the open's leader region is the grown one");
                entry.close_section(open, &mut b, &m, true).unwrap();
            } else {
                entry.close_section(open, &mut b, &GrowMerge, true).unwrap();
            }
        }
    }
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    let acc = exec.node().accounting();
    entry.end(exec, true);
    (sink.sorted(), acc)
}

#[test]
fn a_build_far_past_its_share_grows_and_completes_serial_and_at_w8_unadmitted_and_admitted() {
    let rows = BLOCKS * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    let want = vec![vec![rows, sum, sum, sum]];
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let program = Arc::new(p_grow_share(4));
    let mut entry = Entry::new(2);
    for (width, arm, what) in [(0, Arm::Serial, "W0"), (8, Arm::Merge, "W8"), (8, Arm::Phase2, "W8 phase 2")] {
        let (got, acc) = run(&mut entry, &program, &mut derive, lease(Grant::of(1)), width, arm);
        assert_eq!(got, want, "{what} unadmitted: every row, head and tail intact across the grows");
        assert!(acc.past_share > 0 && acc.past_share_bytes > 0, "{what}: the grows are counted ({acc:?})");
    }
    // admitted: the pool armed with room past the grant; the grows draw past G (reserved from the pool), never refused
    let grant = Grant::of(SHARE_FLOOR as u64);
    let need = footprint(grant, &program, 8);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(need + (256 << 20));
    for (width, arm, what) in [(0, Arm::Serial, "W0"), (8, Arm::Merge, "W8"), (8, Arm::Phase2, "W8 phase 2")] {
        let res = std::rc::Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, 8), Duration::from_millis(10)).unwrap());
        // serially, G's room held but 1 MiB (the slot's open): the leader's grows cannot fit inside G and draw past it
        let held = (width == 0).then(|| res.hold(res.left().saturating_sub(1 << 20)).expect("G's room held"));
        let (got, acc) = run(&mut entry, &program, &mut derive, res.clone(), width, arm);
        drop(held);
        assert_eq!(got, want, "{what} admitted at a {} B grant: every row", grant.bytes);
        assert!(acc.past_share > 0, "{what} admitted: the grows are counted");
        assert!(width != 0 || res.grown() > 0, "W0 admitted: the leader's grows are reserved past G");
    }
}
