//! D1708 (a) (D1702's aliasing class, the half batch 25 left open): a nested run took the runner's derive from the frame's
//! own pointer (gravity_parallel's `Leader`: `&mut *self.derive`, `&mut *self.recheck`; exec's producers: `&mut
//! *self.registry`) while every arm between the runner and the call it re-entered from held its own `&mut` of them across
//! that call (`run_arm`, `run_arm_now`; exec's `run_producer`), a protected borrow. The eager Init's reader that runs as a
//! section runs the Init's members ahead from its late step (`Plan::late_words` -> `OnDemand::prerun_members`, D1035
//! step 2, as exec's `m8_init` does), each member at its width; the first section open provisions the worker slots from
//! the derive (`Node::provision` -> `Derive::derive_source`, which counts its derives as the server's `HostRegistry` does,
//! hosts registry.rs `next += 1`): a write foreign to the reader's arm's protected borrow. The run goes through
//! `run_program_rechecking` over a concrete derive, as the server's does (exec drive.rs over `&mut HostRegistry`): Tree
//! Borrows retags and protects a `&mut` of a concrete (`Unpin`) derive, never a `&mut dyn Derive` (`run_program`'s, whose
//! pointee may be `!Unpin`). Plain `cargo test` passes either way; the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_parallel --test d1708_prerun_derive`. Width 2, a few
//! rows (Miri's speed); the kernels reach their site only through `site_ptr` (a `&mut Site` held across a host call is the
//! kernels' own class, D1705).
mod common;

use core::cell::Cell;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::ctx::site_ptr;
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{ArenaBox, ArenaRef, Binding, Derive, Error, Frame, Locker, Merge, Result, SourceBox, Writer};
use gravity_kernels::par_scan_count::{BYTES, LAYOUT};
use gravity_kernels::util::{consumed, emit, flush, partial, range_seek, window};
use gravity_parallel::{FixturePool, MorselSource, Plan};
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const BLOCKS: u64 = 4;
const PER_BLOCK: u64 = 3;
const ROWS: u64 = BLOCKS * PER_BLOCK;

/// Site 0's rows left in its window, the next window when it is spent (`util::advance`'s protocol over the raw site).
unsafe fn left(ctx: *mut Ctx) -> i64 {
    let s = site_ptr(ctx, 0);
    loop {
        let (cursor, end) = ((*s).cursor, (*s).end());
        if cursor < end {
            return (end - cursor) as i64;
        }
        let n = window(ctx, 0);
        if n <= 0 {
            if n == 0 {
                (*s).state = gravity_abi::site::site::STATE_EOF;
            }
            return n;
        }
    }
}

/// The member (pipeline 0) and the reader (1): the claim's rows of site 0 counted into the pipeline's one-record table.
unsafe extern "C-unwind" fn k_count(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    loop {
        let r = left(ctx);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single().add_count(0, r);
        (*site_ptr(ctx, 0)).cursor += r as u32;
    }
}

/// The emit (2): the member's count and the reader's.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let a = HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0);
    let b = HashAgg::at(consumed(ctx, 1), &LAYOUT, 1, BYTES).single().count(0);
    let v = [a as u64, b as u64];
    let n = [0u8; 2];
    if emit(ctx, 2, &[(v.as_ptr(), n.as_ptr(), 2)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program() -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_A, &[1])];
    let out = [OutCol::new("member", 20, -1, 8, true), OutCol::new("reader", 20, -1, 8, true)];
    let count = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::eligible(0, count(0), ROWS), Code::hand(k_count)),
        (Pipeline::eligible(0, count(1), ROWS), Code::hand(k_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 2, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// The fixture derive, counting its source derives as the server's registry does (`HostRegistry::derive_source`).
struct Counted {
    fixture: FixtureDerive,
    next: u64,
}

impl Derive for Counted {
    fn size_hint(&self, desc: &SiteDesc) -> usize {
        self.fixture.size_hint(desc)
    }
    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        self.next += 1;
        self.fixture.derive_source(desc, a)
    }
    fn writer_size_hint(&self) -> usize {
        self.fixture.writer_size_hint()
    }
    fn derive_writer(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer> {
        self.fixture.derive_writer(plan, a)
    }
    fn locker_size_hint(&self) -> usize {
        self.fixture.locker_size_hint()
    }
    fn derive_locker(&mut self, plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker> {
        self.fixture.derive_locker(plan, a)
    }
}

/// Width 2 throughout; pipeline 0 an eager Init's member (deferred), pipeline 1 its reader: the reader's late step runs the
/// member ahead through the frame published to the plan, once.
struct Ahead {
    inner: FixturePlan,
    frame: Cell<Option<Frame>>,
    ran: Cell<bool>,
}

impl Plan for Ahead {
    fn width_of(&self, _p: usize) -> u16 {
        2
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        self.inner.merge_for(p)
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        self.inner.after_pipeline(p, exec)
    }
    fn deferred(&self, p: usize) -> bool {
        p == 0
    }
    fn late_words(&self, p: usize) -> Result<gravity_parallel::LateWords> {
        if p == 1 && !self.ran.replace(true) {
            let f = self.frame.get();
            let Some(mut f) = f else { return Err(Error::host("no frame published to the plan")) };
            // SAFETY: the frame published to this plan (`publish`), alive while published; this thread's only.
            if !unsafe { f.as_mut() }.prerun_members(1, &[0])? {
                return Err(Error::host("a reader that runs as a section did not run its Init ahead"));
            }
        }
        Ok(gravity_parallel::LateWords::none())
    }
    fn frame(&self, f: Option<Frame>) -> Option<Frame> {
        self.frame.replace(f)
    }
}

/// One statement: the emit's rows and the derives the run made.
fn run() -> Result<(Vec<Vec<u64>>, u64)> {
    let table = FixtureTable::new(T_A, BLOCKS, PER_BLOCK, gen_a);
    let fixture = FixtureDerive::new(&[&table]);
    let program = Arc::new(program());
    let plan = Ahead { inner: FixturePlan::new(&program, &fixture, 2, 1), frame: Cell::new(None), ran: Cell::new(false) };
    let mut derive = Counted { fixture, next: 0 };
    let pool = FixturePool::new(2);
    let mut entry = gravity_driver::Entry::new(2);
    let mut sink = LegSink::new(true);
    let nulls = [0u8; 1];
    let mut b = Binding::new(parts(&[0], &nulls, &NO_ABORT), &mut sink);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(16 << 20)))?;
    let mut recheck = |_: &mut gravity_driver::Entry, _: &mut gravity_driver::Exec, _: &mut Binding<'_>, _: &mut Counted, site: u8| -> Result<bool> { Err(Error::Recheck(site)) };
    let r = gravity_parallel::run_program_rechecking(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan, &mut recheck);
    entry.end(exec, r.is_ok());
    r?;
    assert!(plan.ran.get(), "the reader's late step ran");
    Ok((sink.sorted(), derive.next))
}

#[test]
fn an_inits_run_ahead_opens_its_members_section_through_the_derive_every_outer_arm_holds_raw() {
    let (rows, derives) = run().unwrap();
    assert_eq!(rows, vec![vec![ROWS, ROWS]], "the member ran ahead whole at width 2, then the reader");
    assert_eq!(derives, 3, "the leader's source at the start, then one per worker slot at the member's section open");
}
