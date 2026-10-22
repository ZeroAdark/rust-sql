//! D3412: a breaker a serial owner folds is not raised to its slot's learned peak. That peak is the owner's fold, which the
//! footprint already pins beside the slot (twice the slot's bytes); raised, a rerun of a grouping with history reserved
//! three times its peak (3150 MiB for a 1 GiB fold), and three such two-key groupings asking at once waited past a 60 s
//! statement timeout on a 9.5 GiB pool that seats their first runs.
use gravity_driver_rig as common;

use std::time::Duration;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_driver::mem::feedback::{self, Key, Sample, Source};
use gravity_driver::{footprint, Grant, POOL};
use gravity_program::{Arena, Code, End, Layout, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// A hash aggregate that never starts small (no spill road) whose serial owner folds slot 0, then the emit.
fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None });
    let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    let pipes = vec![
        (Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, b, 1000) }, Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_emit_groups_spill)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes)
}

#[test]
fn a_serial_owners_learned_peak_leaves_its_rerun_as_its_first_run() {
    let program = program();
    let fold_kib = 1u32 << 20;
    let now = feedback::now();
    let key = Key { program: program.front_key().0, db: 0, slot: 0, participants: feedback::participants(1) };
    for grant in [Grant::of(64 << 20), Grant::of(64 << 20).with_ceilings(1 << 30, 2 << 30)] {
        let first = footprint(grant, &program, 1);
        feedback::record(&key, Sample { peak: fold_kib, need: fold_kib, given: fold_kib, at: now }, 0, Source::Est, now, false);
        let (id, no_history) = feedback::begin(program.key.0, program.front_key().0, 1, 0, [(0u8, false, false)].into_iter(), u64::MAX).expect("a live entry");
        assert!(!no_history, "slot 0 has its learned peak");
        let rerun = footprint(grant, &program, 1);
        POOL.arm(first + (64 << 20));
        let admitted = POOL.admit_need(grant, &|g| footprint(g, &program, 1), Duration::from_millis(50)).map(|r| r.reserved);
        POOL.arm(0);
        feedback::end(id);
        assert_eq!(rerun, first, "{grant:?}: the rerun reserves what its first run did");
        assert_eq!(admitted.ok(), Some(first), "{grant:?}: a pool that seats the first run seats the rerun whole");
    }
}
