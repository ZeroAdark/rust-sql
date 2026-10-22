//! D4054: a hash build's append.arms run whose stream spilled. A v4 build that outgrows its share writes its own partition
//! files into its slot's stream and reads them back in its pair pass; its merge has no owner phase. The driver read the
//! stream's partition count as a record table's for every arm of a run (D393b: a keyed end's earlier arms scatter their
//! records there), so the run's last arm started an owner fold over the build's files: no owner at any size, and the
//! statement refused at the one-owner wall. A build's run now finishes as a lone build does: the merge's finish, the seal.
//! Here: two arms of one build slot, the first writing a partition as a build does, at width 1 (the last arm finishes
//! on the leader, `run_arm`) and at width 4 (the last arm runs as a section, whose close asks the stream's bits first:
//! `run_section`).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::ctx::site_ptr;
use gravity_abi::status::{CANCELLED, DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{Error, Flush, Grant, Merge, Partial, Region, Result, SpillFile, State};
use gravity_kernels::util::{advance, consumed, emit, end, flush, interrupted, range_seek};
use gravity_parallel::{FixturePool, MorselSource, Plan};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// The build's partition bits (its breaker's and its files').
const BITS: u8 = 3;
const MARK_AT: usize = 256;
const MARK: u64 = 0x0D40_54D4_054D_4054;

/// A build's merge (lowerv4's `BuildMerge` in shape): a serial fold of each partial and a finish, no owner phase. An
/// owner fold over it is the defect: it raises by name where the trait's default answers "no room at this size".
#[derive(Default)]
struct Build {
    merged: AtomicUsize,
    finished: AtomicUsize,
}

impl Merge for Build {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        self.merged.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn finish(&self, leader: &mut State<'_>) -> Result<()> {
        self.finished.fetch_add(1, Ordering::Relaxed);
        leader.grow(MARK_AT + 8)?;
        leader.bytes[MARK_AT..MARK_AT + 8].copy_from_slice(&MARK.to_le_bytes());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn own_partition(&self, _owner: Region, _part: u32, _pbits: u8, _partials: &[Partial<'_>], _files: &[&dyn SpillFile], _site: *mut gravity_abi::Site, _cancel: *const u8, _round: u8, _flush: Flush<'_>) -> Result<()> {
        Err(Error::host("an owner fold over a build's partition files"))
    }
}

/// The arm's rows read and dropped (the build's table is not this test's).
unsafe fn scan(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        if interrupted(ctx) {
            return CANCELLED;
        }
        let s = &mut *site_ptr(ctx, 0);
        s.rows_out += (end(s) - s.cursor) as u64;
        s.cursor = end(s);
    }
}

/// The run's first arm: one record written to partition 0 of its own slot's stream, as a v4 build's lowered code writes a
/// partition it spilled (driver `spill_host::v4`), then its rows.
unsafe extern "C-unwind" fn k_arm_spills(ctx: *mut Ctx) -> i64 {
    let rec = [0x5Au8; 16];
    if v4::put(ctx, 0, BITS, 0, &rec) < 0 || v4::flush(ctx, 0, BITS) < 0 {
        return ERROR;
    }
    scan(ctx)
}

/// The run's last arm: its rows.
unsafe extern "C-unwind" fn k_arm(ctx: *mut Ctx) -> i64 {
    scan(ctx)
}

/// The finish's mark read back from the consumed slot.
unsafe extern "C-unwind" fn k_emit_mark(ctx: *mut Ctx) -> i64 {
    let v = [core::ptr::read_unaligned(consumed(ctx, 0).add(MARK_AT) as *const u64)];
    let n = [0u8; 1];
    if emit(ctx, 1, &[(v.as_ptr(), n.as_ptr(), 1)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// Two arms of build slot 0 over `rel` (an Arms proto over them), then the emit that reads the slot.
fn p_build_run(rel: u32, rows: u64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1, 2])];
    let out = [OutCol::new("mark", 20, -1, 8, true)];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: BITS } });
    let pipes = [
        (Pipeline::eligible(0, b, rows), Code::hand(k_arm_spills)),
        (Pipeline::eligible(0, b, rows), Code::hand(k_arm)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_emit_mark)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes);
    p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(2)]));
    p
}

struct Building(FixturePlan, Build);

impl Plan for Building {
    fn width_of(&self, p: usize) -> u16 {
        self.0.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.0.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        if p < 2 { &self.1 } else { self.0.merge_for(p) }
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        self.0.after_pipeline(p, exec)
    }
}

/// The run at width `w`: served, the first arm's partition file in the slot's stream, one finish (the last arm's), and
/// at width 2 and up its partials folded by the merge (the arms ran as sections).
fn served_at(w: u16) {
    install_mem_spill();
    let table = FixtureTable::new(T_A, 64, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_build_run(T_A, 64 * 50));
    let pool = FixturePool::new(4);
    let mut entry = gravity_driver::Entry::new(2);
    let plan = Building(FixturePlan::new(&program, &derive, w, 3), Build::default());
    let mut sink = LegSink::new(true);
    let r = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, Grant::of(64 << 20), &[0u64], &NO_ABORT, &mut sink);
    let (rows, spilled, parts, _) = r.unwrap_or_else(|e| panic!("width {w}: the run's last arm finishes with no owner fold and no refusal: {e:?}"));
    assert!(spilled && parts == 1 << BITS, "width {w}: the first arm's partition file is the slot's stream ({parts} partitions)");
    assert_eq!(rows, 1, "width {w}: served");
    assert_eq!(sink.sorted(), vec![vec![MARK]], "width {w}: the finish's mark in the region the consumer reads");
    assert_eq!(plan.1.finished.load(Ordering::Relaxed), 1, "width {w}: one finish, the last arm's");
    if w > 1 {
        assert!(plan.1.merged.load(Ordering::Relaxed) > 0, "width {w}: the arms ran as sections (their partials folded)");
    }
}

/// The last arm finishes on the leader (`run_arm`: `owner_bits_of`, then `own_single_of`).
#[test]
fn a_builds_run_whose_stream_spilled_finishes_its_last_arm_on_the_leader() {
    served_at(1);
}

/// The last arm runs as a section (`run_section`: `Section::spill_bits`, then the one owner on this thread).
#[test]
fn a_builds_run_whose_stream_spilled_finishes_its_last_arm_as_a_section() {
    served_at(4);
}
