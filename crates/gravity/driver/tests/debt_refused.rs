//! A refused grow or open of a region whose breaker or road names a SPILL-DEBT row (`grow::debt_refused`) is the same out
//! of memory (53200 at exec glue) with the row in `Error::OutOfMemory::why`, which glue writes in the server log only: a
//! resident build's (`BuildNestInner`) grow on the leader (`region_grow`, `State::grow`) and on a worker (`grow_partial`)
//! and its slot's open, a spilling build's road that names its row (`region_grow_debt`, `State::grow_debt`:
//! `BuildSpillMerge`). A spilling region's refused open names none. A grow its wait refused for the statement's cancel is the
//! cancel (57014), as any error a host holds for a canceled call is. Under a pool armed at the admission's need and no
//! more. One test: it arms the process-wide pool.
use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{footprint, Call, Entry, Error, Grant, Merge, Partial, Result, State, DEPOT, POOL, REGIONS};
use gravity_program::spill::{Holds, NoSpill, Proof};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const MIB: u64 = 1 << 20;
/// Past G and past the pool's room: every grow below is refused.
const GROW: u64 = 64 * MIB;
const SPILLS: Holds = Holds::Spills(Spill::Partitioned { bits: 3 });
const DEBT: Holds = Holds::Bounded(Proof::Estimate(NoSpill::BuildNestInner));

/// The statement's abort byte.
static ABORT: AtomicU8 = AtomicU8::new(0);
/// `k_grow` cancels its statement before it grows.
static CANCEL_IN_CALL: AtomicBool = AtomicBool::new(false);

/// A plain grow of the produced region (`SPILL_REGION_GROW`); ERROR when refused (the driver holds the error).
unsafe extern "C-unwind" fn k_grow(ctx: *mut Ctx) -> i64 {
    if CANCEL_IN_CALL.load(Relaxed) {
        ABORT.store(gravity_abi::ctx::ABORT_BIT, Relaxed);
    }
    // SAFETY: the driver binds `ctx` for the call; its pipeline produces a region; head and tail 0
    if unsafe { gravity_abi::program::region_grow(ctx, GROW, 0, 0) } == 0 { ERROR } else { DONE }
}

/// The grow on a spilling build's road that names its row (`SPILL_REGION_GROW_DEBT`).
unsafe extern "C-unwind" fn k_grow_road(ctx: *mut Ctx) -> i64 {
    // SAFETY: as `k_grow`
    if unsafe { gravity_abi::program::region_grow_debt(ctx, GROW, 0, 0, NoSpill::BuildSpillMerge.code()) } == 0 { ERROR } else { DONE }
}

/// The section's merge grows the leader's slot (`State::grow`, or `State::grow_debt` on `road`).
struct GrowMerge(Option<NoSpill>);

impl Merge for GrowMerge {
    fn merge_into(&self, leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        match self.0 {
            Some(w) => leader.grow_debt(GROW as usize, None, w),
            None => leader.grow(GROW as usize),
        }
    }
}

/// A lowered program: a share-budgeted hash build of slot 0 (its kernel `k`) that `holds` past its grant, then the emit.
fn program(holds: Holds, k: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline { holds: Some(holds), ..Pipeline::eligible(0, b, 1000) }, Code::hand(k)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_noop)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::SELECT, pipes).with_used(1)
}

/// The producer of `program` run by the leader (`width` 0) or by a section of `width` closed with `merge`, admitted under a
/// pool armed at its need and no more (`held_g`: G's room held whole too, so the slot's open is refused): its error.
fn refused(program: Program, width: u16, merge: &dyn Merge, held_g: bool) -> Option<Error> {
    let program = Arc::new(program);
    let w = width.max(1);
    POOL.gate(0);
    REGIONS.trim();
    DEPOT.trim();
    let grant = Grant::of(MIB);
    POOL.arm(footprint(grant, &program, w));
    let res = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, w), Duration::from_millis(10)).expect("admitted"));
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &ABORT);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");
    let carved = held_g.then(|| res.hold(res.left()).expect("G's room held"));
    let r = if width == 0 {
        entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).map(|_| ())
    } else {
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, width).expect("the section opens");
        let claims = (0..width).try_for_each(|o| open.run_claim(o, MorselRange::new(2 * o as u64, 2 * o as u64 + 2)).map(|_| ()));
        let closed = entry.close_section(open, &mut b, merge, claims.is_ok());
        claims.and(closed)
    };
    ABORT.store(0, Relaxed);
    entry.end(exec, r.is_ok());
    drop((carved, b, res, entry));
    REGIONS.trim();
    DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
    r.err()
}

fn why(e: &Option<Error>) -> Option<NoSpill> {
    match e {
        Some(Error::OutOfMemory { why, .. }) => *why,
        other => panic!("a refused grow or open is an out of memory: {other:?}"),
    }
}

#[test]
fn a_refused_debt_region_names_its_row_and_a_canceled_wait_is_the_cancel() {
    let none = GrowMerge(None);
    // the resident build: its grow on the leader (the IR's and the merge's) and on a worker, and its slot's open
    assert_eq!(why(&refused(program(DEBT, k_grow), 0, &none, false)), Some(NoSpill::BuildNestInner), "W0: the leader's grow names the build's row");
    assert_eq!(why(&refused(program(DEBT, k_grow), 2, &none, false)), Some(NoSpill::BuildNestInner), "W2: a worker's grow names it");
    assert_eq!(why(&refused(program(DEBT, k_noop), 2, &none, false)), Some(NoSpill::BuildNestInner), "W2: the merge's State::grow names it");
    assert_eq!(why(&refused(program(DEBT, k_noop), 0, &none, true)), Some(NoSpill::BuildNestInner), "W0: the slot's refused open names it");
    // a spilling build: its road names its own row; its refused open names none
    assert_eq!(why(&refused(program(SPILLS, k_grow_road), 0, &none, false)), Some(NoSpill::BuildSpillMerge), "W0: the road's row");
    assert_eq!(why(&refused(program(SPILLS, k_noop), 2, &GrowMerge(Some(NoSpill::BuildSpillMerge)), false)), Some(NoSpill::BuildSpillMerge), "W2: State::grow_debt's row");
    assert_eq!(why(&refused(program(SPILLS, k_noop), 0, &none, true)), None, "a spilling region's refused open: a plain out of memory");
    // the statement canceled while its grow waits on the pool: the cancel, never an out of memory
    CANCEL_IN_CALL.store(true, Relaxed);
    let e = refused(program(DEBT, k_grow), 0, &none, false);
    CANCEL_IN_CALL.store(false, Relaxed);
    assert_eq!(e, Some(Error::Cancelled), "the canceled wait's refusal is the cancel");
}

/// The reason rides in `OutOfMemory`'s padding: an `Error` is as large as before.
#[test]
fn an_error_is_no_larger() {
    assert_eq!(core::mem::size_of::<Error>(), 32);
}
