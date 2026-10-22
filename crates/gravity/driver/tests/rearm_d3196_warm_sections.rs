//! D3196 (rearm E1): a rerun set's member that runs as a section (D3014 S2', D3137) opens one per rescan, and each open
//! bound every host of every worker slot again on the leader (each close unbound them). A nested run's section now leaves
//! its slots' hosts bound, warm for its binding (`WorkerSlot::warm`): the next open under it (the next rescan's, another
//! member's, at any width) binds none; a wider open binds only the slots past the warm ones. A failed section unbinds its
//! slots; the statement's end unbinds every warm one. No source is ever bound twice without an unbind between, and a warm
//! slot's sources are re-armed (`Source::rearm`) at each open that keeps them.
//! Pipelines: 0 the member (a count over the range site, a section at the test's width), 1 the owner (reads slot 0); the
//! two-member program: 0 and 1 members (a count over sites 0 and 1, slots 0 and 1), 2 the owner.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::status::DONE;
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Binding, Claimed, Entry, Error, Exec, Grant, Open};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(3196), generation: Generation(1) };
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const CLAIM: u64 = 5;
/// `k_range_count` counts the values below the first parameter: every row.
const ALL: [u64; 1] = [u64::MAX];

unsafe extern "C-unwind" fn k_owner(_ctx: *mut Ctx) -> i64 {
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let member = Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), 1000);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]);
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    let p = Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, [(member, Code::hand(k_range_count)), (owner, Code::hand(k_owner))]);
    rerun_set(p, &[0])
}

/// Two members of one rerun set (PostgreSQL's two SubPlans of one sublink read twice, as `sum(s), count(s)`), each a count
/// over its own range site into its own slot, and the owner reading both.
fn two_members() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2]), SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let agg = |slot: u8| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
    let a = Pipeline::eligible(0, agg(0), 1000);
    let b = Pipeline::eligible(1, agg(1), 1000);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]);
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    let p = Program::with_pipelines(Layout { nsites: 2, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, [(a, Code::hand(k_range_count)), (b, Code::hand(k_range_count_s1)), (owner, Code::hand(k_owner))]);
    rerun_set(p, &[0, 1])
}

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(CLAIM, Relaxed);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

/// Member `p`'s run: one section at `width`, every ordinal draining the claims in turn.
fn section(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, derive: &mut FakeDerive, p: usize, width: u16) -> gravity_driver::Result<()> {
    let open = e.open_section(x, b, derive, p, width)?;
    let cursor = AtomicU64::new(0);
    let claimed = (0..width).map(|o| drain(&open, &cursor, o)).collect::<Result<Vec<u64>, Error>>();
    let ok = claimed.is_ok();
    e.close_section(open, b, &CountMerge, ok)?;
    claimed.map(|_| ())
}

/// The count in the set's slot `slot` now.
fn slot_count(exec: &Exec, slot: u8) -> i64 {
    // SAFETY: the slot is live (the owner reads it), `COUNT_SUM_BYTES` of it laid by its member's breaker.
    unsafe { count_sum(exec.node().slot_region(slot).as_mut_ptr()).count(0) }
}

/// The tests of this binary one at a time: the slot depot and REGIONS are process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn a_rerun_sections_slots_stay_bound_across_rescans_and_unbind_at_the_statements_end() {
    let _g = serial();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&ALL, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 1).unwrap();
    let leader = table.binds.load(Relaxed);
    assert_eq!(table.bound_now.load(Relaxed), leader as i64, "the leader's source alone is bound before any rescan");
    for i in 0..200 {
        entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 2)).unwrap();
        assert_eq!(slot_count(&exec, 0), (BLOCKS * PER_BLOCK) as i64, "rescan {i} counts its whole relation");
        assert_eq!(table.binds.load(Relaxed), leader + 2, "rescan {i}: the two worker slots were bound at the first rescan's open alone");
        assert_eq!(table.bound_now.load(Relaxed), leader as i64 + 2, "rescan {i}: the two warm slots stay bound between rescans");
        assert_eq!(table.rearms.load(Relaxed), 2 * i as u64, "rescan {i}: each warm slot's source re-armed at every open after the first");
    }
    // a wider rescan: the two warm slots kept (re-armed), the third bound
    entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 3)).unwrap();
    assert_eq!(slot_count(&exec, 0), (BLOCKS * PER_BLOCK) as i64);
    assert_eq!(table.binds.load(Relaxed), leader + 3, "a wider open binds only the slot past the warm ones");
    assert_eq!(table.bound_now.load(Relaxed), leader as i64 + 3);
    assert_eq!(table.rearms.load(Relaxed), 2 * 199 + 2);
    entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 3)).unwrap();
    assert_eq!(table.binds.load(Relaxed), leader + 3, "the next rescan at that width binds none");
    assert_eq!(table.rearms.load(Relaxed), 2 * 199 + 2 + 3);
    // back to two: slots 0 and 1 kept; slot 2 stays warm (idle), unbound at the statement's end
    entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 2)).unwrap();
    assert_eq!(slot_count(&exec, 0), (BLOCKS * PER_BLOCK) as i64);
    assert_eq!(table.binds.load(Relaxed), leader + 3);
    assert_eq!(table.bound_now.load(Relaxed), leader as i64 + 3);
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), gravity_driver::Step::Done, "the owner runs over the last rescan's slot");
    entry.end(exec, true);
    assert_eq!(table.bound_now.load(Relaxed), 0, "the statement's end unbinds the warm slots with the leader");
    assert_eq!(table.rebinds.load(Relaxed), 0, "no source bound twice without an unbind between");
    assert_eq!(table.pins.load(Relaxed), 0);
    assert_eq!(table.leader_image.load(Relaxed), ALL.len() as u64, "the leader's source binds from the statement's image");
    assert_eq!(table.worker_images.load(Relaxed), 0, "a worker slot's source binds from no parameter image");
}

#[test]
fn a_failed_rescans_section_unbinds_its_slots() {
    let _g = serial();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&ALL, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 1).unwrap();
    let leader = table.binds.load(Relaxed);
    for _ in 0..3 {
        entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 2)).unwrap();
    }
    assert_eq!(table.bound_now.load(Relaxed), leader as i64 + 2);
    table.fail_block.store(7, Relaxed);
    let r = entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, _| section(e, x, bd, &mut derive, 0, 2));
    assert!(r.is_err(), "a read error in a later rescan fails it");
    assert_eq!(table.binds.load(Relaxed), leader + 2, "the failed rescan's open kept its warm slots");
    assert_eq!(table.bound_now.load(Relaxed), leader as i64, "the failed section unbound its slots");
    entry.end(exec, false);
    assert_eq!(table.bound_now.load(Relaxed), 0);
    assert_eq!(table.rebinds.load(Relaxed), 0);
    assert_eq!(table.pins.load(Relaxed), 0);
}

#[test]
fn two_members_of_each_rescan_share_the_warm_slots() {
    let _g = serial();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(two_members());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&ALL, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 2).unwrap();
    entry.skip_rerun(&mut exec, 2).unwrap();
    let leader = table.binds.load(Relaxed);
    assert_eq!(table.bound_now.load(Relaxed), leader as i64, "the leader's sources alone are bound before any rescan");
    for i in 0..50u64 {
        entry.rerun_nested(&mut exec, &mut b, &[0, 1], &mut |e, x, bd, p| section(e, x, bd, &mut derive, p, 2)).unwrap();
        assert_eq!((slot_count(&exec, 0), slot_count(&exec, 1)), ((BLOCKS * PER_BLOCK) as i64, (BLOCKS * PER_BLOCK) as i64), "rescan {i}: each member counts its whole relation");
        assert_eq!(table.binds.load(Relaxed), leader + 4, "rescan {i}: two slots of two sources each, bound at the first member's first open alone");
        assert_eq!(table.bound_now.load(Relaxed), leader as i64 + 4, "rescan {i}: the second member's open keeps the first's warm slots");
        assert_eq!(table.rearms.load(Relaxed), 8 * i + 4, "rescan {i}: every open after the first re-arms the slots' four sources");
    }
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), gravity_driver::Step::Done, "the owner reads both slots");
    entry.end(exec, true);
    assert_eq!(table.bound_now.load(Relaxed), 0);
    assert_eq!(table.rebinds.load(Relaxed), 0);
    assert_eq!(table.pins.load(Relaxed), 0);
}
