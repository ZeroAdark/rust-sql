//! D4170: an owner fold over a merge with no owner phase (`Merge::own_partition`'s default, or a hash aggregate's merge
//! with no owner functions, whose open refuses) refuses by name at once. The fold read kind 2 as a region too small: over
//! the default it pinned its region again at twice the size up to the one-owner wall, and over a refused open it sought a
//! floor no region holds; both then refused with the wall's text, which names a size.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_driver::{no_owner_phase, Entry, Error, Grant, Merge, Partial, Region, Result, State, OWN_PINNED, TEST_OWNER_WALL};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// A partitioned hash aggregate's producer of slot 0, then the emit that reads it.
fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline::eligible(0, b, 1000), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_emit_groups_spill)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes)
}

/// A hash aggregate's merge with no owner functions (parallel `HashAggPartial`): its owner open refuses.
struct NoOwnerFns;

impl Merge for NoOwnerFns {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }

    fn open_owner(&self, _owner: Region, _ordinal: u16) -> Result<()> {
        Err(no_owner_phase())
    }
}

/// `merge`'s one owner over producer 0's slot: its answer and the owner regions it pinned.
fn own(merge: &dyn Merge) -> (Result<()>, u64) {
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [7u64, 3];
    let mut b = binding(&params, &[0, 0], &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    let pinned = OWN_PINNED.load(Ordering::Relaxed);
    let r = entry.own_single_of(&mut exec, 0, merge, 3);
    let pins = OWN_PINNED.load(Ordering::Relaxed) - pinned;
    entry.end(exec, false);
    (r, pins)
}

/// One test: the pin counter is the process's.
#[test]
fn an_owner_fold_over_a_merge_with_no_owner_phase_refuses_by_name_and_pins_once() {
    // a wall the regrowth reaches in a few doublings
    TEST_OWNER_WALL.store(1 << 20, Ordering::Relaxed);
    for (what, merge) in [("the default own_partition", &NoMerge as &dyn Merge), ("a refused owner open", &NoOwnerFns)] {
        let (r, pins) = own(merge);
        assert!(matches!(&r, Err(Error::Refused(t)) if t.contains("no owner phase")), "{what}: a refusal that names the missing owner phase, not a size: {r:?} ({pins} pins)");
        assert!(pins <= 1, "{what}: the owner region pinned once, never grown ({pins} pins)");
    }
}
