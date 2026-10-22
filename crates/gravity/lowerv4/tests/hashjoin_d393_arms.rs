//! D393 (V4R-47): the arms of a streamed UNION ALL under a fold are one production. Each arm is its own producer pipeline
//! ending in the same PlainAgg slot; the v4 `Arms` protocol names the run and its slot, and the driver opens the slot at
//! the first arm and folds the later arms on into it (node.rs `arms_of`): the slot is neither re-zeroed nor freed between
//! the arms. Without the protocol a later producer of the slot opens it afresh (the last arm's rows only: the bug the
//! lowerer's guard stood for).
#[allow(dead_code, unused_imports, unused_variables)]
#[path = "../../driver/tests/common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::MorselRange;
use gravity_driver::{Call, Claimed, Entry, Error, Grant, Open, Step};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(5), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const CLAIM: u64 = 2;
/// The keyed arms' relation: 20000 rows, 6000 groups past a floored share's table (the spill test's shape).
const KBLOCKS: u64 = 400;
const KPER_BLOCK: u64 = 50;

/// Two arms over the same relation folding count and sum into slot 0, then the emit of the fold; with `arms` the v4
/// `Arms` protocol over pipelines 0..2 naming slot 0.
fn p_fold_arms(arms: bool) -> Program {
    let st = Arena::new(1 << 14);
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let fold = || (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), 1000), Code::hand(k_range_count_w));
    let pipes = [fold(), fold(), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_emit_count))];
    let mut p = Program::with_pipelines(
        Layout { nsites: 1, out_natts: 2, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 },
        &sites,
        &[OutCol::new("c0", 20, -1, 8, true), OutCol::new("c1", 20, -1, 8, true)],
        WritePlan::SELECT,
        pipes,
    );
    if arms {
        let mut proto = Proto::new(ProtoKind::Arms, 0);
        proto.arg = 2;
        proto.slot = 0;
        p.set_v4(&V4Descs::new(vec![proto]));
    }
    p
}

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

/// Every pipeline serially: the emit's row.
fn serial(program: &Arc<Program>) -> Vec<Vec<u64>> {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let params = [u64::MAX];
    let mut sink = LegSink::new();
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    for _ in 0..3 {
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    }
    assert!(!exec.node().slot_live(0), "freed after the emit");
    entry.end(exec, true);
    sink.sorted()
}

#[test]
fn a_folds_arms_continue_one_slot() {
    let rows = BLOCKS * PER_BLOCK;
    let sum: u64 = (0..rows).sum();
    assert_eq!(serial(&Arc::new(p_fold_arms(true))), vec![vec![2 * rows, 2 * sum]], "both arms folded into the one record");
    assert_eq!(serial(&Arc::new(p_fold_arms(false))), vec![vec![rows, sum]], "no Arms protocol: the second producer reopens the slot");
}

/// Pipeline `p` as a section of width 3 over the whole relation.
fn section(entry: &mut Entry, exec: &mut gravity_driver::Exec, b: &mut gravity_driver::Binding<'_>, derive: &mut FakeDerive, p: usize) {
    let open = entry.open_section(exec, b, derive, p, 3).unwrap();
    let cursor = AtomicU64::new(0);
    for o in 0..3 {
        drain(&open, &cursor, o).unwrap();
    }
    entry.close_section(open, b, &CountMerge, true).unwrap();
}

/// Arm 0 / arm 1 as a section (`true`) or serially: the section path frees and marks its pipeline in its own order.
fn mixed(arms: [bool; 2]) -> Vec<Vec<u64>> {
    let program = Arc::new(p_fold_arms(true));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let params = [u64::MAX];
    let mut sink = LegSink::new();
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    for (p, s) in arms.into_iter().enumerate() {
        if s {
            section(&mut entry, &mut exec, &mut b, &mut derive, p);
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        }
        assert!(exec.node().slot_live(0), "live across the arms (after arm {p})");
    }
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert!(!exec.node().slot_live(0));
    entry.end(exec, true);
    sink.sorted()
}

#[test]
fn a_folds_arms_continue_one_slot_across_sections() {
    let rows = BLOCKS * PER_BLOCK;
    let sum: u64 = (0..rows).sum();
    for arms in [[false, true], [true, false], [true, true]] {
        assert_eq!(mixed(arms), vec![vec![2 * rows, 2 * sum]], "arms as sections: {arms:?}");
    }
}

/// D393b (V4R-57): a keyed end over two arms, `GROUP BY v % $0` spilling by partition past its share: arm 0 over site 0, arm 1
/// over site 1 (the same relation), both into slot 0 under an Arms proto, then the emit that refills the partitions.
fn p_group_arms() -> Program {
    p_group_arms_as(gravity_abi::breaker::Spill::Partitioned { bits: 3 })
}

/// `p_group_arms` with the breaker's spill `spill` (F-ARMS: `Spill::None`, a resident keyed slot as a set side declares).
fn p_group_arms_as(spill: gravity_abi::breaker::Spill) -> Program {
    let st = Arena::new(1 << 14);
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2]), SiteDesc::range_source(a, 2002, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: gravity_abi::breaker::Budget::Share, spill };
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(1, End::Breaker(breaker), 1000), Code::hand(k_range_group_spill_1)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_emit_groups_spill)),
    ];
    let cols: [OutCol; 3] = core::array::from_fn(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true));
    let mut p = Program::with_pipelines(Layout { nsites: 2, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes);
    let mut proto = Proto::new(ProtoKind::Arms, 0);
    proto.arg = 2;
    proto.slot = 0;
    p.set_v4(&V4Descs::new(vec![proto]));
    p
}

/// The groups of `p_group_arms` with arm `i` as a width-3 section when `arms[i]`, else serial, under `grant`.
fn keyed(arms: [bool; 2], grant: Grant, modulus: u64) -> (Vec<Vec<u64>>, bool) {
    keyed_as(p_group_arms(), arms, grant, modulus)
}

/// `keyed` over `program`.
fn keyed_as(program: Program, arms: [bool; 2], grant: Grant, modulus: u64) -> (Vec<Vec<u64>>, bool) {
    let program = Arc::new(program);
    let table = RangeTable::new(KBLOCKS, KPER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let params = [modulus, 3];
    let nulls = [0u8; 2];
    let mut sink = LegSink::new();
    let mut b = binding(&params, &nulls, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(grant)).unwrap();
    for (p, s) in arms.into_iter().enumerate() {
        if s {
            let open = entry.open_section(&mut exec, &b, &mut derive, p, 3).unwrap();
            let cursor = AtomicU64::new(0);
            for o in 0..3 {
                loop {
                    let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
                    if lo >= KBLOCKS || open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(KBLOCKS))).unwrap() == Claimed::Stopped {
                        break;
                    }
                }
            }
            entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        }
    }
    let spilled = exec.node().spilled();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    (sink.sorted(), spilled)
}

#[test]
fn a_keyed_ends_arms_own_every_arms_groups() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gravity_driver::install_spill_factory(&MEM_SPILL);
    });
    let rows = KBLOCKS * KPER_BLOCK;
    let modulus = 6000u64;
    // every value v in 0..rows once per arm: group g holds the v with v % 6000 == g, twice
    let mut want: Vec<Vec<u64>> = (0..modulus).map(|g| {
        let vs: Vec<u64> = (0..rows).filter(|v| v % modulus == g).collect();
        vec![g, 2 * vs.len() as u64, 2 * vs.iter().sum::<u64>()]
    }).collect();
    want.sort();
    for grant in [Grant::of(64 << 20), Grant::of(1)] {
        for arms in [[false, false], [false, true], [true, false], [true, true]] {
            let (got, spilled) = keyed(arms, grant, modulus);
            assert_eq!(got, want, "arms as sections {arms:?}, grant {} B", grant.bytes);
            // D2648: a serial arm that continues its run (D393b) keeps its groups in the leader's table while the grant
            // holds them; a section's arm, or the leader's table at a section's open, goes to the slot's files; at 1 B the
            // arms spill too
            let spills = grant.bytes < 64 << 20 || arms.iter().any(|&s| s);
            assert_eq!(spilled, spills, "the first arm's groups went to the files: {spilled} ({arms:?}, grant {} B)", grant.bytes);
        }
    }
}

/// F-ARMS / D1238: a resident keyed end (`Spill::None`: a set side's, SETOP row 2, or a key-copying group's, D1352) over two
/// arms owns every arm's groups, each arm serial or a section: a continuing section's arm hands each worker's phase-1 table
/// to the slot's stream (driver `Node::hand_off`, in memory under the roomy grant: no spill), a section after a serial arm
/// hands the leader's table first (`hand_leader`), and the last arm's owner phase or own_single folds them. A roomy grant only:
/// the hand kernel scatters when its table fills (`SPILL_SCATTER`, a partitioned end's move), which a resident end never
/// does (compiled ends grow the region, D254's `region_grow`); the e2e set sides over UNION ALL arms at a small work_mem
/// cover the grown resident region.
#[test]
fn a_resident_keyed_ends_arms_own_every_arms_groups() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gravity_driver::install_spill_factory(&MEM_SPILL);
    });
    let rows = KBLOCKS * KPER_BLOCK;
    let modulus = 6000u64;
    let mut want: Vec<Vec<u64>> = (0..modulus).map(|g| {
        let vs: Vec<u64> = (0..rows).filter(|v| v % modulus == g).collect();
        vec![g, 2 * vs.len() as u64, 2 * vs.iter().sum::<u64>()]
    }).collect(); // alloc: Cold
    want.sort();
    for grant in [Grant::of(64 << 20)] {
        for arms in [[false, false], [false, true], [true, false], [true, true]] {
            let (got, spilled) = keyed_as(p_group_arms_as(gravity_abi::breaker::Spill::None), arms, grant, modulus);
            if got != want {
                let first = got.iter().zip(want.iter()).position(|(g, w)| g != w);
                panic!("resident arms {arms:?}, grant {} B: {} groups, want {}; first difference {:?}: got {:?} want {:?}; spilled {spilled}", grant.bytes, got.len(), want.len(), first, first.map(|i| &got[i]), first.map(|i| &want[i]));
            }
            assert!(!spilled, "a resident slot never spills ({arms:?}, grant {} B)", grant.bytes);
        }
    }
}
