//! D4030: a pool worker's claim lends its statement's abort byte to the claim's host code, as a Node's call does (D3887,
//! node/raw.rs `lent`) and an owner phase (D3902, `lend_cancel`). `Open::claim_on` called the program bare, so every long
//! host loop's poll (`cancelpoll::canceled`) on a W >= 2 worker read no byte: a W8 statement's cancel waited out its
//! workers' host calls. TPC-H q10 at W8 on a cold part cache answered a pg_cancel_backend 1.7-4.8 s late, its 8 workers in
//! the granule source's text dictionary builds (`row_codes`) and its leader parked in `run_led`. In memtest's d3982-wm1g
//! stress, the watchdog's cancel of such a statement (pid 1101) never answered before the kernel's OOM kill. Width 2, one
//! claim per ordinal: the kernel asks `cancelpoll::canceled()` from the claim, as a host loop inside it would. With the
//! binding's abort byte raised every claim sees the cancel, and with it clear none does.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::DONE;
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Claimed, Entry, Grant, Merge, Partial, Result, State};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const PER_BLOCK: u64 = 64;
const BLOCKS: u64 = 8;
const NULLS: [u8; 1] = [0];

/// Claims run, and claims whose host code saw the cancel.
static ASKED: AtomicUsize = AtomicUsize::new(0);
static SAW: AtomicUsize = AtomicUsize::new(0);

/// A claim's kernel: asks the host's cancel poll once (a long host loop's question) and stops.
unsafe extern "C-unwind" fn k_ask(_ctx: *mut Ctx) -> i64 {
    ASKED.fetch_add(1, Ordering::Relaxed);
    if cancelpoll::canceled() {
        SAW.fetch_add(1, Ordering::Relaxed);
    }
    DONE
}

unsafe extern "C-unwind" fn k_none(_ctx: *mut Ctx) -> i64 {
    DONE
}

struct NoMerge;

impl Merge for NoMerge {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }
}

fn program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("n", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_ask)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_none))];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

#[test]
fn a_workers_claim_lends_its_statements_abort_byte_to_its_host_code() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    // no cancel: no claim's host code sees one
    assert_eq!(open.run_claim(0, MorselRange::new(0, 2)).unwrap(), Claimed::Done);
    assert_eq!((ASKED.load(Ordering::Relaxed), SAW.load(Ordering::Relaxed)), (1, 0), "no cancel pending, none seen");
    // the statement canceled (pg_cancel_backend, statement_timeout or the memory watchdog raise the byte's abort bit):
    // each worker's claim sees it from its host code
    flag.store(cancelpoll::ABORT_BIT, Ordering::SeqCst);
    assert_eq!(open.run_claim(0, MorselRange::new(2, 4)).unwrap(), Claimed::Done);
    assert_eq!(open.run_claim(1, MorselRange::new(4, 6)).unwrap(), Claimed::Done);
    assert_eq!((ASKED.load(Ordering::Relaxed), SAW.load(Ordering::Relaxed)), (3, 2), "both workers' claims saw the statement's cancel from their host code");
    flag.store(0, Ordering::SeqCst);
    // outside a claim no byte is lent: a host loop there reads none
    assert!(!cancelpoll::canceled(), "the claim's byte is not left lent after it");
    // the canceled statement's section closes failed and the statement ends failed, as the runner closes them
    let _ = entry.close_section(open, &mut b, &NoMerge, false);
    entry.end(exec, false);
}
