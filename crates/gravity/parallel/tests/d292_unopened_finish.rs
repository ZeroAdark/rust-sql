//! D292 (main 09-24, found by M1-JOIN): a width-1 producer that no claim reached (an empty relation, a `run_empty` mask) ran
//! its merge's finish on a leader slot that was never opened; `end_claims` opened it only after. A finish that grows the
//! leader and writes its header (JOIN's build: PHASE / FORM EMPTY) wrote a region `end_claims` then opened over, zeroed, so
//! the consumer read no header (m1_d276_empty_build at W0: 22012 where PG answers 0); on the next statement over the
//! parked node the slot's settled size made `State::grow` answer "fits" over the unopened (null) slot and the header went
//! through the empty view (W8: SIGSEGV). The slot now opens before the merge touches it, and `State::grow` on an unopened
//! slot is a named ERROR. Here: one producer over an empty relation whose merge's finish grows the leader and writes a mark,
//! and an emit that reads the mark back from the consumed slot; four statements over one parked node at width 1.

mod common;

use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Entry, Grant, Merge, Result, State};
use gravity_kernels::util::{consumed, emit, flush};
use gravity_parallel::{run_program, FixturePool, MorselSource, Plan};
use gravity_program::{Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(292), generation: Generation(1) };
const MARK_AT: usize = 256;
const MARK: u64 = 0x0D29_2D29_2D29_2D29;

/// A finish-only merge (JOIN's build merge's shape): the leader grown to hold the mark, the mark written.
struct Marks;

impl Merge for Marks {
    fn finish(&self, leader: &mut State<'_>) -> Result<()> {
        leader.grow(MARK_AT + 8)?;
        leader.bytes[MARK_AT..MARK_AT + 8].copy_from_slice(&MARK.to_le_bytes());
        Ok(())
    }
}

/// The mark read back from the consumed slot.
unsafe extern "C-unwind" fn k_emit_mark(ctx: *mut Ctx) -> i64 {
    let v = [core::ptr::read_unaligned(consumed(ctx, 0).add(MARK_AT) as *const u64)];
    let n = [0u8; 1];
    if emit(ctx, 1, &[(v.as_ptr(), n.as_ptr(), 1)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn p_mark(rel: u32) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1, 2])];
    let out = [OutCol::new("mark", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(breaker), 1), Code::hand(k_group_share)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_emit_mark)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

struct Marking(FixturePlan);

impl Plan for Marking {
    fn width_of(&self, p: usize) -> u16 {
        self.0.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.0.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        if p == 0 { &Marks } else { self.0.merge_for(p) }
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        self.0.after_pipeline(p, exec)
    }
}

#[test]
fn an_unclaimed_width_1_producers_finish_writes_the_region_its_consumer_reads_on_every_statement_of_a_parked_node() {
    let table = FixtureTable::new(T_A, 0, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_mark(T_A));
    let pool = FixturePool::new(2);
    let mut entry = Entry::new(2);
    for stmt in 0..4 {
        table.fresh();
        let plan = Marking(FixturePlan::new(&program, &derive, 1, 3));
        let mut sink = LegSink::new(true);
        let (params, nulls) = ([0u64], [0u8; 1]);
        let mut b = Binding::new(parts(&params, &nulls, &NO_ABORT), &mut sink);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).expect("started");
        let r = run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
        entry.end(exec, r.is_ok());
        assert_eq!(r, Ok(1), "statement {stmt}: served");
        assert_eq!(sink.sorted(), vec![vec![MARK]], "statement {stmt}: the finish's mark in the region the consumer reads");
    }
    assert_eq!(entry.stats().built, 1, "one node, parked across every statement");
}
