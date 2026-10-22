//! D2378 (CAND-M26): an InitPlan's members run nested inside a call of their reader (`Entry::run_nested`, reached from the
//! reader's `m8_init_run` through the published current) run whole. The eager Init's one reader stays eligible (D1035
//! step 2) and runs by claims, and a member's range source seeks `ctx.claim` at its open: the member's call
//! (`run_pipeline`) kept the reader's claim there, so the InitPlan scanned only the reader's blocks, and
//! `count(*) .. WHERE EXISTS (an uncorrelated scan)` whose first qualifying row lay past them answered 0 (PostgreSQL 300).
//! A pipeline call now reads no claim, and the reader's claim is back after the run (`hold` / `reattach`).
//! Pipelines: 0 the member (the InitPlan's plan: its own relation's scan, ending in the sink as an Init cell end does),
//! 1 the reader (driving site 1, run by one claim).
use gravity_driver_rig as common;

use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::{next_row, seek, site};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, CurrentCell, Entry, Exec, Grant, MorselRange, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(43), generation: Generation(1) };
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
/// The reader's claim: two of the member relation's 24 blocks.
const CLAIM: MorselRange = MorselRange::new(5, 7);

/// The runner's current-execution slot, for the reader's nested run (the host's `m8_init_run` reaches it so).
static CELL: AtomicPtr<CurrentCell> = AtomicPtr::new(core::ptr::null_mut());
/// The claim the member's call read and the rows its scan returned; the reader's claim after the run.
static MEMBER_LO: AtomicU64 = AtomicU64::new(0);
static MEMBER_HI: AtomicU64 = AtomicU64::new(0);
static MEMBER_ROWS: AtomicU64 = AtomicU64::new(0);
static READER_LO: AtomicU64 = AtomicU64::new(0);
static READER_HI: AtomicU64 = AtomicU64::new(0);

/// A range source's open over site `i` (its claim as its keys, then the seek), then its rows counted to the end.
unsafe fn scan(ctx: *mut Ctx, i: usize) -> i64 {
    let s = &mut *site_ptr(ctx, i);
    (*s.keys, *s.keys.add(1), *s.keys_null, *s.keys_null.add(1)) = ((*ctx).claim.lo, (*ctx).claim.hi, 0, 0);
    seek(s);
    let mut n = 0;
    loop {
        let r = next_row(s, 2);
        if r < 0 {
            return r;
        }
        if r == 0 {
            s.state = site::STATE_EOF;
            return n;
        }
        n += 1;
    }
}

/// The member: the InitPlan's scan of its own relation (site 0).
unsafe extern "C-unwind" fn k_member(ctx: *mut Ctx) -> i64 {
    MEMBER_LO.store((*ctx).claim.lo, Relaxed);
    MEMBER_HI.store((*ctx).claim.hi, Relaxed);
    let n = scan(ctx, 0);
    if n < 0 {
        return n;
    }
    MEMBER_ROWS.store(n as u64, Relaxed);
    DONE
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("member {p} stopped with {s:?}"),
    }
}

/// The reader: its claim's scan of site 1, and inside the call, as its first read of the InitPlan's param does, the
/// member run nested through the current execution; then the claim it reads after the run.
unsafe extern "C-unwind" fn k_reader(ctx: *mut Ctx) -> i64 {
    if scan(ctx, 1) < 0 {
        return ERROR;
    }
    let Some(cur) = (*CELL.load(Relaxed)).get() else { return ERROR };
    if (*cur.entry).run_nested(&mut *cur.exec, &mut *cur.binding, &[0], &mut run_whole).is_err() {
        return ERROR;
    }
    READER_LO.store((*ctx).claim.lo, Relaxed);
    READER_HI.store((*ctx).claim.hi, Relaxed);
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2]), SiteDesc::range_source(st.handle(), 2002, &[1, 2])];
    let member = Pipeline::serial(0, End::Sink, Reason::Host);
    let reader = Pipeline::serial(1, End::Sink, Reason::Host);
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    Program::with_pipelines(Layout { nsites: 2, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, [(member, Code::hand(k_member)), (reader, Code::hand(k_reader))])
}

#[test]
fn an_init_run_inside_a_readers_claim_scans_its_whole_relation_and_the_claim_comes_back() {
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    let cell: CurrentCell = Cell::new(None);
    CELL.store(&cell as *const CurrentCell as *mut CurrentCell, Relaxed);
    let step = entry.with_current_slot(&cell, |e| e.run_claim(&mut exec, &mut b, CLAIM));
    CELL.store(core::ptr::null_mut(), Relaxed);
    assert_eq!(step.unwrap(), Step::Done);
    assert_eq!((MEMBER_LO.load(Relaxed), MEMBER_HI.load(Relaxed)), (MorselRange::ALL.lo, MorselRange::ALL.hi), "the member's call reads no claim of the reader's");
    assert_eq!(MEMBER_ROWS.load(Relaxed), BLOCKS * PER_BLOCK, "the InitPlan's scan reads its whole relation, not the reader's two blocks");
    assert_eq!((READER_LO.load(Relaxed), READER_HI.load(Relaxed)), (CLAIM.lo, CLAIM.hi), "the reader's claim is back after the run");
    entry.end(exec, true);
}
