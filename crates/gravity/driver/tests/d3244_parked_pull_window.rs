//! D3244: a once-read CTE pulled one row per resume inside its reader's call, its plan drawing a by-reference image per row
//! from the arena scratch (m6_agg_final's copy). Each parked resume's chunks stay pinned to the pull (D2344 (b)), but the
//! reader's emptied window was where the next resume began, so every resume took a fresh 64 KiB chunk for its 16 B
//! (q39b SF10: 172,618 chunks, 10.8 GB; here 2,000). The parked member now bumps on in its own window: the chunks stay
//! few and every image any resume drew reads back unchanged to the end.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::{arena_alloc, resume, site_ptr, RESUME_NONE};
use gravity_abi::site::{next_row, seek, site};
use gravity_abi::status::{suspend, DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Call, Entry, Exec, Grant, Pulled, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(32), generation: Generation(44) };
const BLOCKS: u64 = 40;
const PER_BLOCK: u64 = 50;
const ROWS: u64 = BLOCKS * PER_BLOCK;
const IMAGE: usize = 16;
const THR: [u64; 1] = [1000];
const NULLS: [u8; 1] = [0];

static GROWS: AtomicU64 = AtomicU64::new(0);
static BAD: AtomicU64 = AtomicU64::new(0);
static LANDED: AtomicU64 = AtomicU64::new(0);
static READER_STOP: AtomicU8 = AtomicU8::new(1);
static READ: AtomicI64 = AtomicI64::new(-1);
/// Every image the pulled plan drew: its address and the row it stamped.
static IMAGES: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

fn intact() -> bool {
    IMAGES.lock().unwrap().iter().all(|&(p, v)| unsafe { core::slice::from_raw_parts(p as *const u8, IMAGE) }.iter().all(|&b| b == v as u8))
}

/// The set's producer (pipeline 0).
unsafe extern "C-unwind" fn k_prod(ctx: *mut Ctx) -> i64 {
    k_range_count(ctx)
}

/// The pulled plan (1): one row per call into its store, its image drawn from the scratch, then a resumable stop.
unsafe extern "C-unwind" fn k_pull(ctx: *mut Ctx) -> i64 {
    let s = &mut *site_ptr(ctx, 1);
    if resume::phase((*ctx).resume) != resume::EMIT {
        (*s.keys, *s.keys.add(1), *s.keys_null, *s.keys_null.add(1)) = ((*ctx).claim.lo, (*ctx).claim.hi, 0, 0);
        seek(s);
    }
    if !intact() {
        BAD.fetch_add(1, Relaxed);
    }
    let r = next_row(s, 2);
    if r < 0 {
        return r;
    }
    if r == 0 {
        (s.state, (*ctx).resume) = (site::STATE_EOF, RESUME_NONE);
        return DONE;
    }
    let v = *s.values;
    let end = (*ctx).arena_end;
    let p = arena_alloc(ctx, IMAGE);
    if p.is_null() {
        return ERROR;
    }
    if (*ctx).arena_end != end {
        GROWS.fetch_add(1, Relaxed);
    }
    core::ptr::write_bytes(p, v as u8, IMAGE);
    IMAGES.lock().unwrap().push((p as usize, v));
    let store = count_sum(own(ctx));
    store.add_count(0, 1);
    store.add_i64(1, v as i64);
    LANDED.fetch_add(1, Relaxed);
    (*ctx).resume = resume::word(resume::EMIT, 0);
    suspend(false)
}

/// The reader (2, the CTE Scan): stops at its first call (the pulls run inside its hold), then reads the store's count.
unsafe extern "C-unwind" fn k_read(ctx: *mut Ctx) -> i64 {
    if READER_STOP.load(Relaxed) == 1 && resume::phase((*ctx).resume) != resume::EMIT {
        (*ctx).resume = resume::word(resume::EMIT, 0);
        return suspend(false);
    }
    READ.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    (*ctx).resume = RESUME_NONE;
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2]), SiteDesc::range_source(st.handle(), 2002, &[1, 2])];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::serial(0, agg(1), Reason::Host), Code::hand(k_prod)),
        (Pipeline::serial(1, agg(0), Reason::Host).consuming(&[1]), Code::hand(k_pull)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_read)),
    ];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    Program::with_pipelines(Layout { nsites: 2, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes)
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut gravity_driver::Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("producer {p} stopped with {s:?}"),
    }
}

#[test]
fn a_once_read_cte_pulled_a_row_per_resume_packs_its_scratch_into_a_few_kept_chunks() {
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(256 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    entry.skip_pulled(&mut exec, &[2]).unwrap();
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "the reader opens and stops");
    let mut pulls = 0u64;
    while entry.pull_nested(&mut exec, &mut b, &[0, 1], &mut run_whole).unwrap() == Pulled::Stopped {
        pulls += 1;
        assert_eq!(exec.next_pipeline(), 2, "the reader's place back after each pull");
    }
    assert_eq!((pulls, LANDED.load(Relaxed)), (ROWS, ROWS), "one row per resume, each parked inside the reader's hold");
    assert_eq!(BAD.load(Relaxed), 0, "a resume found an earlier resume's image overwritten");
    assert!(intact(), "an image overwritten after the last resume");
    let bound = 2 + ROWS * IMAGE as u64 / (64 << 10);
    assert!(GROWS.load(Relaxed) <= bound, "{} arena grows for {ROWS} resumes of {IMAGE} B (bound {bound}): a chunk per resume", GROWS.load(Relaxed));
    READER_STOP.store(0, Relaxed);
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap(), Step::Done);
    assert_eq!(READ.load(Relaxed), ROWS as i64, "the reader reads the whole store");
    entry.end(exec, true);
}
