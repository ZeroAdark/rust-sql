use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Call, Claimed, Entry, Error, Grant, Open, Step, REGIONS, SHARE_FLOOR};
use gravity_abi::breaker::{HashAgg};
use gravity_program::{Generation, Handle, PlanKey, Program};

const KEY: PlanKey = PlanKey { handle: Handle(5), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const CLAIM: u64 = 5;
const MIB: u64 = 1 << 20;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

#[test]
fn a_grant_splits_across_live_breakers_then_width_with_floor_and_cap() {
    let g = Grant::of(8 * MIB);
    let s = g.split(2, 4);
    assert_eq!((s.breaker.bytes as u64, s.worker.bytes as u64), (4 * MIB, MIB));
    let one = g.split(1, 1);
    assert_eq!((one.breaker.bytes as u64, one.worker.bytes as u64), (8 * MIB, 8 * MIB));
    let zero = g.split(0, 0);
    assert_eq!(zero, one, "no live breaker / no width divide by one");
    let tiny = Grant::of(1000).split(4, 16);
    assert_eq!((tiny.breaker.bytes, tiny.worker.bytes), (SHARE_FLOOR, SHARE_FLOOR));
    let huge = Grant::of(1 << 40).split(1, 1);
    assert_eq!(huge.breaker.bytes, u32::MAX & !7, "a share fits a u32 region (a multiple of 8)");
    assert_eq!(Grant::from_gucs(4096, 2.0), Grant::of(8 << 20), "PostgreSQL's defaults: 4 MB × 2");
    assert_eq!(Grant::from_gucs(65536, 1.0).bytes, 64 * MIB);
    assert_eq!(Grant::from_gucs(-5, 0.5).bytes, 64 * 1024, "work_mem floors at 64 kB, the multiplier at 1");
}

/// D2587 (main 2026-10-03 18:11Z ruling): a breaker's share is at least PostgreSQL's own budget for its node (a hash node
/// hash_mem, a Sort / top-N / Material work_mem); the split across the live breakers only adds above it. Before, a hash
/// aggregate beside a top-N sort at work_mem 64kB had 64 kB where PostgreSQL gives it 128 kB.
#[test]
fn d2587_a_breaker_share_is_at_least_its_nodes_postgres_budget() {
    use gravity_abi::breaker::BreakerKind;
    const KIB: u64 = 1 << 10;
    let g = Grant::of(128 * KIB).with_floors(64 * KIB, 128 * KIB);
    assert_eq!(g.split(2, 1).breaker.bytes as u64, 64 * KIB, "no floor: the even split");
    assert_eq!(g.split_at_least(2, 1, g.floor_of(BreakerKind::HashAgg)).breaker.bytes as u64, 128 * KIB, "a hash aggregate keeps hash_mem beside a top-N");
    assert_eq!(g.split_at_least(2, 1, g.floor_of(BreakerKind::TopK)).breaker.bytes as u64, 64 * KIB, "the top-N keeps work_mem");
    for k in [BreakerKind::HashBuild, BreakerKind::Distinct] {
        assert_eq!(g.floor_of(k), 128 * KIB, "{k:?}: a hash node");
    }
    for k in [BreakerKind::Sort, BreakerKind::Materialize] {
        assert_eq!(g.floor_of(k), 64 * KIB, "{k:?}: work_mem");
    }
    assert_eq!(g.floor_of(BreakerKind::PlainAgg), 0);
    let w4 = g.split_at_least(2, 4, g.floor_of(BreakerKind::HashAgg));
    assert_eq!(w4.breaker.bytes as u64, 128 * KIB, "the width cuts after the floor");
    assert_eq!(w4.worker.bytes as u64, 128 * KIB, "each worker's own share at least its node's budget, as PG gives each parallel participant");
    // the floors are one node's budget, not the width's: at W8 (grant 8 x 8 MiB, work_mem 4 MB) two live breakers keep the
    // even split's 32 MiB leader share, a worker its 8 MiB hash_mem; a W x budget floor made the leader 64 MiB, serial or not
    let w8 = Grant::of(64 * MIB).with_floors(4 * MIB, 8 * MIB);
    let s = w8.split_at_least(2, 8, w8.floor_of(BreakerKind::HashBuild));
    assert_eq!((s.breaker.bytes as u64, s.worker.bytes as u64), (32 * MIB, 8 * MIB));
    let s = w8.split_at_least(2, 8, w8.floor_of(BreakerKind::Sort));
    assert_eq!((s.breaker.bytes as u64, s.worker.bytes as u64), (32 * MIB, 4 * MIB), "a Sort worker its work_mem");
    for live in [1u16, 2, 3, 8, 64] {
        for width in [1u16, 2, 8, 16] {
            for floor in [0, 64 * KIB, 4 * MIB, 8 * MIB, 64 * MIB] {
                let s = w8.split_at_least(live, width, floor);
                assert!(s.worker.bytes <= s.breaker.bytes && s.owner.bytes <= s.breaker.bytes, "live {live} W{width} floor {floor}: {s:?}");
                assert!(s.breaker.bytes as u64 >= floor.min(u32::MAX as u64 & !7), "live {live} W{width} floor {floor}");
                if width >= 2 {
                    assert!(s.worker.bytes as u64 >= floor.min(s.breaker.bytes as u64), "live {live} W{width} floor {floor}");
                }
            }
        }
    }
    let big = Grant::of(64 * MIB).with_floors(4 * MIB, 8 * MIB);
    assert_eq!(big.split_at_least(2, 1, big.floor_of(BreakerKind::Sort)), big.split(2, 1), "above the floor the split stands");
    assert_eq!(Grant::of(8 << 20).with_floors(0, 0), Grant::from_gucs(4096, 2.0), "from_gucs carries no floors");
}

/// D651: past 8 GiB a breaker's share, the workers split what the leader region (its u32 cap) leaves. Dividing the capped
/// region held every worker to 4 GiB / W whatever the grant: a 16 GiB grant gave a two-key group-by's W16 workers 256 MiB each (they need
/// ~270 MiB) and 8 GiB of it went unused. At 8 GiB and under the split is unchanged (D653 is the 2 x T reservation there).
#[test]
fn d651_past_8_gib_the_workers_split_what_the_leader_leaves() {
    const GIB: u64 = 1 << 30;
    let s = Grant::of(16 * GIB).split(1, 16);
    assert_eq!(s.breaker.bytes, u32::MAX & !7, "the leader region stays one u32 region");
    assert_eq!(s.worker.bytes as u64, 768 * MIB, "16 GiB less the 4 GiB leader, 16 ways");
    assert_eq!(Grant::of(32 * GIB).split(2, 16).worker.bytes as u64, 768 * MIB, "per live breaker");
    assert_eq!(Grant::of(64 * GIB).split(1, 8).worker.bytes, u32::MAX & !7, "a worker's own region is capped");
    for grant in [8 * GIB + 8, 9 * GIB, 12 * GIB, 16 * GIB, 64 * GIB, 1 << 40] {
        for width in 2..=17u16 {
            let fp = gravity_driver::regions_of(Grant::of(grant), 1, width);
            assert!(fp <= grant, "grant {grant} W{width}: the regions {fp} fit the grant");
            let s = Grant::of(grant).split(1, width);
            assert!(s.worker.bytes <= s.breaker.bytes);
            let c = Grant::of(grant).split_for(1, width, true);
            assert_eq!(c.worker.bytes + c.images.bytes, s.worker.bytes, "the image cut comes out of the worker share");
        }
    }
    for grant in [8 * MIB, 64 * MIB, (1 << 30) / 3, 4 * GIB, 6 * GIB, 8 * GIB] {
        for (live, width) in [(1u16, 1u16), (1, 2), (1, 8), (1, 16), (2, 16), (3, 9)] {
            let s = Grant::of(grant).split(live, width);
            let breaker = (grant / live as u64).clamp(SHARE_FLOOR as u64, u32::MAX as u64) & !7;
            let worker = (breaker / width as u64).max(SHARE_FLOOR as u64) & !7;
            assert_eq!((s.breaker.bytes as u64, s.worker.bytes as u64), (breaker, worker), "grant {grant} live {live} W{width}: unchanged at 8 GiB and under");
        }
    }
}

/// D335: every share is a multiple of 8. A join build keeps its payload images down from its region's end (`LIMIT`), so an
/// odd worker share (64 MiB over 3 live breakers, then 9 ways at W8) put every copied numeric at an odd address.
#[test]
fn every_share_a_grant_splits_is_a_multiple_of_8() {
    for grant in [64 * MIB, 8 * MIB, 64 * MIB + 1000, (1 << 30) / 3, 1 << 40] {
        for live in 1..=8u16 {
            for width in 1..=17u16 {
                for images in [false, true] {
                    let s = Grant::of(grant).split_for(live, width, images);
                    for (what, b) in [("breaker", s.breaker.bytes), ("worker", s.worker.bytes), ("owner", s.owner.bytes), ("images", s.images.bytes)] {
                        assert_eq!(b % 8, 0, "grant {grant} live {live} width {width} images {images}: the {what} share {b} is not a multiple of 8");
                    }
                }
            }
        }
    }
}

fn groups(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, grant: Grant, width: u16) -> (Vec<Vec<u64>>, gravity_driver::Accounting, Vec<(u32, u32)>, (u32, u32)) {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [8u64];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(grant)).unwrap();
    let mut workers = Vec::new();
    if width >= 2 {
        let open = entry.open_section(&mut exec, &b, derive, 0, width).unwrap();
        let cursor = AtomicU64::new(0);
        // round-robin claims so every ordinal builds a partial
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
            let t = unsafe { HashAgg::view(open.partial(o).bytes.as_ptr()) };
            workers.push((t.cap, t.limit));
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
    } else {
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    }
    let leader = unsafe { HashAgg::view(exec.node().slot_region(0).as_ptr()) };
    let leader = (leader.cap, leader.limit);
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    let acc = exec.node().accounting();
    entry.end(exec, true);
    (sink.sorted(), acc, workers, leader)
}

#[test]
fn a_four_mb_grant_sizes_the_tables_from_the_share() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_share(64));
    assert!(program.slots.shared[0] && program.slots.bytes[0] == 0 && program.slots.max_live == 1);
    let mut entry = Entry::new(2);
    let (serial, acc1, _, leader1) = groups(&mut entry, &program, &mut derive, Grant::of(4 * MIB), 1);
    assert_eq!(serial.len(), 8, "eight groups of v % 8");
    assert_eq!(serial.iter().map(|r| r[1]).sum::<u64>(), BLOCKS * PER_BLOCK);
    assert_eq!(leader1, (16, 4 * MIB as u32), "serially the leader's table is the breaker share: 4 MB / 1 live breaker (eight groups grow 4 → 8 → 16 at 7/8 load)");
    assert_eq!((acc1.opens, acc1.frees, acc1.armed, acc1.largest_share as u64, acc1.grown_bytes), (1, 1, 1, 4 * MIB, 4 * MIB));
    let (par, acc2, workers, leader2) = groups(&mut entry, &program, &mut derive, Grant::of(4 * MIB), 4);
    assert_eq!(par, serial, "identical groups at W = 4");
    assert_eq!(workers, vec![(16, MIB as u32); 4], "each worker's table grew 4 → 16 within a 1 MB share (4 MB / 1 breaker / 4 workers)");
    assert_eq!(leader2, (16, 4 * MIB as u32));
    assert_eq!((acc2.opens, acc2.frees, acc2.armed), (2, 2, 1 + 1 + 4), "the parked Node's counters carry over: one more open, the leader and four workers armed");
    assert_eq!(acc2.grown_bytes, 4 * MIB + 4 * MIB, "the leader region was already 4 MB; four worker regions of 1 MB were grown once");
    let (again, acc3, _, _) = groups(&mut entry, &program, &mut derive, Grant::of(4 * MIB), 4);
    assert_eq!(again, serial);
    assert_eq!(acc3.grown_bytes, acc2.grown_bytes, "the same share grows nothing more");
    let (small, acc4, workers, leader) = groups(&mut entry, &program, &mut derive, Grant::of(1), 2);
    assert_eq!(small, serial);
    assert_eq!((leader.1, workers[0].1, workers[1].1), (SHARE_FLOOR, SHARE_FLOOR, SHARE_FLOOR), "a grant below the floor arms the floor");
    assert_eq!(acc4.grown_bytes, acc3.grown_bytes, "a smaller share reuses the larger regions");
    let (big, acc5, workers, leader) = groups(&mut entry, &program, &mut derive, Grant::of(64 * MIB), 2);
    assert_eq!(big, serial);
    assert_eq!((leader.1 as u64, workers[0].1 as u64), (64 * MIB, 32 * MIB));
    assert_eq!(acc5.grown_bytes, acc4.grown_bytes + 64 * MIB + 2 * 32 * MIB, "a larger share grows the leader and two worker regions once");
    assert_eq!(acc5.largest_share as u64, 64 * MIB);
}

/// D222: at statement end a Node's regions go to the process-wide depot, where another statement may take them (the
/// gate's concurrent tests did, a 64 kB floor ask popping a 1 MB class). The re-run maps those again; the census counts
/// a Node growing to a larger share, so the same share still grows nothing, leader or worker.
#[test]
fn a_region_another_statement_took_from_the_depot_is_retaken_not_grown() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_share(64));
    let mut entry = Entry::new(2);
    let (first, acc1, _, _) = groups(&mut entry, &program, &mut derive, Grant::of(4 * MIB), 4);
    assert_eq!(acc1.grown_bytes, 4 * MIB + 4 * MIB, "the leader's 4 MB and four worker regions of 1 MB");
    // another statement drains the depot: the leader's class and every 1 MB region this Node parked
    let taken: Vec<_> = (0..4).map(|_| REGIONS.take(MIB as usize, None).unwrap()).chain([REGIONS.take(4 * MIB as usize, None).unwrap()]).collect();
    let (again, acc2, workers, leader) = groups(&mut entry, &program, &mut derive, Grant::of(4 * MIB), 4);
    assert_eq!(again, first);
    assert_eq!((workers, leader), (vec![(16, MIB as u32); 4], (16, 4 * MIB as u32)), "the same tables from re-taken regions");
    assert_eq!(acc2.grown_bytes, acc1.grown_bytes, "the same share grows nothing, the depot emptied under it or not");
    drop(taken);
}

#[test]
fn a_share_too_small_for_the_groups_refuses_instead_of_overrunning() {
    let table = RangeTable::new(200, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_share(64));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [100_000u64];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let r = entry.run_pipeline(&mut exec, &mut b, Call::batch(64));
    let cap = HashAgg::cap_of(&GROUP, SHARE_FLOOR);
    assert!((table.rows() as u32) > cap, "2000 distinct keys against a {cap}-record floor table");
    assert_eq!(r, Err(Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH)), "past the share the insert refuses (the spill's hook, commit 4)");
    entry.end(exec, false);
}

#[test]
fn slots_open_at_their_producer_and_free_after_their_last_consumer() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_two_slots(64));
    assert_eq!((program.slots.bytes[0], program.slots.bytes[1], program.slots.max_live), (COUNT_SUM_BYTES, COUNT_SUM_BYTES, 2));
    assert_eq!((0..3).map(|p| program.slots.live_mask(p)).collect::<Vec<_>>(), vec![0b01, 0b11, 0b11]);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let params = [u64::MAX];
    let rows = BLOCKS * PER_BLOCK;
    let sum: u64 = (0..rows).sum();
    for cycle in 0..2 {
        let mut sink = LegSink::new();
        let mut b = binding(&params, &NULLS, &mut sink, &flag);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let node = exec.node();
        assert!(!node.slot_live(0) && !node.slot_live(1), "cycle {cycle}: nothing live at start");
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        assert!(exec.node().slot_live(0) && !exec.node().slot_live(1), "slot 0 stays live for pipeline 2");
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        assert!(exec.node().slot_live(0) && exec.node().slot_live(1));
        assert_eq!(unsafe { count_sum(exec.node().slot_region(1).as_mut_ptr()) }.count(0), rows as i64, "cycle {cycle}: the region was zeroed at open (a parked Node keeps the last execution's bytes)");
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        assert!(!exec.node().slot_live(0) && !exec.node().slot_live(1), "both freed after their last consumer");
        let acc = exec.node().accounting();
        assert_eq!((acc.opens, acc.frees, acc.armed), (2 * (cycle + 1), 2 * (cycle + 1), 0), "program-constant slots are never armed");
        entry.end(exec, true);
        assert_eq!(sink.sorted(), vec![vec![2 * rows, 2 * sum]], "cycle {cycle}: the emit read both consumed regions");
    }
    // the first producer as a section, the second serially: the same regions, the same answer
    let mut sink = LegSink::new();
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 3).unwrap();
    let cursor = AtomicU64::new(0);
    for o in 0..3 {
        drain(&open, &cursor, o).unwrap();
    }
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    assert!(exec.node().slot_live(0));
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert!(!exec.node().slot_live(0) && !exec.node().slot_live(1));
    entry.end(exec, true);
    assert_eq!(sink.sorted(), vec![vec![2 * rows, 2 * sum]]);
}
