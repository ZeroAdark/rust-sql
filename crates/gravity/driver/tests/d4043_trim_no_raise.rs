//! D4043: a trimmed grant takes no learned raise. Under pool pressure the admission halves a statement's grant
//! (`Grant::trimmed`, which drops the floors); its breaker shares were still raised to the memory history's learned
//! starts, uncapped because a trim has no budgets. A W4 sort trimmed to 64 MiB of 16384 MiB asked opened a 1.7 GiB
//! leader and four 1 GiB worker shares (reserved 5.8 GiB): halving never lowered the need, the floor road admitted it
//! alone past the pool, and the first open past G's remainder was the unnamed 'Failed on request of size 1073741824
//! in a Gravity breaker region' (cell e2, 8G). A trim's shares are its own bytes' split; a grant that never had
//! budgets (`Grant::of`) and an estimate's grant with its floors keep their raise as before.
use gravity_driver_rig as common;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_driver::mem::feedback::{self, Key, Sample, Source};
use gravity_driver::mem::grant::{share, StartSize};
use gravity_driver::{footprint, Grant};
use gravity_program::{Arena, Code, End, Layout, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

const MIB: u64 = 1 << 20;
const WIDTH: u16 = 4;

/// A parallel hash aggregate that never starts small (no spill road) whose owner folds slot 0, then the emit.
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
fn a_trimmed_grant_opens_its_own_split_whatever_the_history_learned() {
    let program = program();
    // the admission's grant at the cap (work_mem 2GB, hash_mem_multiplier 2) and the pool's trim of it to the floor
    let want = Grant::of(16 << 30).with_floors(2 << 30, 4 << 30).with_ceilings(2 << 30, 4 << 30).with_start(StartSize::Share);
    let trim = want.trimmed(64 * MIB);
    let fixed = Grant::of(64 * MIB);
    let (cold, cold_need, cold_fixed) = (share(&program, 0, trim, WIDTH), footprint(trim, &program, WIDTH), share(&program, 0, fixed, WIDTH));
    // the slot's learned start: 1 GiB for the leader and for each worker
    let now = feedback::now();
    let start_kib = 1u32 << 20;
    for slot in [0u16, 0x8000] {
        let key = Key { program: program.front_key().0, db: 0, slot, participants: feedback::participants(WIDTH) };
        feedback::record(&key, Sample { peak: start_kib, need: start_kib, given: start_kib, at: now }, 0, Source::Est, now, false);
    }
    let (id, no_history) = feedback::begin(program.key.0, program.front_key().0, WIDTH, 0, [(0u8, true, false)].into_iter(), u64::MAX).expect("a live entry");
    let (warm, warm_need, warm_fixed) = (share(&program, 0, trim, WIDTH), footprint(trim, &program, WIDTH), share(&program, 0, fixed, WIDTH));
    feedback::end(id);
    assert!(!no_history, "slot 0 has its learned start");
    // the history is live: a grant that never had budgets keeps the raise as learned (unchanged)
    assert!(warm_fixed.breaker.bytes as u64 > 512 * MIB && warm_fixed.worker.bytes as u64 > 512 * MIB, "Grant::of keeps its learned raise: {cold_fixed:?} -> {warm_fixed:?}");
    assert!(cold.breaker.bytes as u64 <= 64 * MIB, "the trim's own leader share: {cold:?}");
    assert_eq!(warm, cold, "a 64 MiB trim opens its own split, not the learned 1 GiB starts");
    assert_eq!(warm_need, cold_need, "a 64 MiB trim reserves its own footprint ({} MiB), not the learned starts' ({} MiB)", cold_need >> 20, warm_need >> 20);
}
