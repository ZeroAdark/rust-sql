//! D1707 (D1706 (b)'s pattern on the parallel workers): `open_section` attached each worker slot's binding through its
//! bind-time borrow of the slot (`slot.binding.as_mut()`), and every claim reborrowed the slot anew (`Open::run_claim`'s
//! `slot`, `claim_on`'s, `partial`'s): a read of the binding under the attached pointer, which froze it once a settle
//! (`add_rows`: a SELECT counts its rows) had written through it, so an ordinal's next claim wrote its rows through a
//! frozen pointer. Plain `cargo test` passes either way; the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_driver --test d1707_claim_binding`. Width 2, three
//! claims per ordinal, a few rows (Miri's speed). The kernel reaches its site only through `site_ptr`: a `&mut Site` held
//! across a host call is the kernels' own class (D1705 / D1711), not this one.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::{cancel_pending, site_ptr};
use gravity_abi::site::{next_row, seek, site};
use gravity_abi::status::{CANCELLED, DONE, ERROR};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Claimed, Entry};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const BLOCKS: u64 = 6;
const PER_BLOCK: u64 = 3;
const NULLS: [u8; 1] = [0];

/// `count(*), sum(row)` over the claim's blocks into the slot's one-record table; site 0 reached through `site_ptr` only.
unsafe extern "C-unwind" fn k_count(ctx: *mut Ctx) -> i64 {
    let s = site_ptr(ctx, 0);
    *(*s).keys = (*ctx).claim.lo;
    *(*s).keys.add(1) = (*ctx).claim.hi;
    *(*s).keys_null = 0;
    *(*s).keys_null.add(1) = 0;
    seek(s);
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, 0), 2);
        if r < 0 {
            return r;
        }
        let s = site_ptr(ctx, 0);
        if r == 0 {
            (*s).state = site::STATE_EOF;
            return DONE;
        }
        let agg = count_sum(own(ctx));
        agg.add_count(0, 1);
        agg.add_i64(1, *(*s).values as i64);
    }
}

/// The emit: the slot's `count, sum`, one row.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let agg = count_sum(own(ctx));
    let (count, sum) = (agg.count(0) as u64, agg.i64(1) as u64);
    let o = (*ctx).out;
    *(*o).values = count;
    *(*o).values.add(1) = sum;
    *(*o).isnull = 0;
    *(*o).isnull.add(1) = 0;
    (*o).count = 1;
    if gravity_abi::program::push(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("count", 20, -1, 8, true), OutCol::new("sum", 20, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES)), 1000), Code::hand(k_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 2, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

#[test]
fn an_ordinals_later_claims_settle_through_their_own_pointer_to_the_slots_binding() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    // one block per claim, the ordinals alternating: each ordinal's second and third claim settle over the binding its
    // first claim's settle wrote
    for lo in 0..BLOCKS {
        assert_eq!(open.run_claim((lo % 2) as u16, MorselRange::new(lo, lo + 1)).unwrap(), Claimed::Done, "claim {lo}");
    }
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    entry.run_to_end(&mut exec, &mut b).unwrap();
    entry.end(exec, true);
    let rows = BLOCKS * PER_BLOCK;
    assert_eq!(sink.sorted(), vec![vec![rows, (0..rows).sum::<u64>()]], "every claim's rows counted once");
    assert_eq!(table.pins.load(std::sync::atomic::Ordering::Relaxed), 0, "every claim released its page");
}
