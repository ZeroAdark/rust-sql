//! D1712 (D1707's class, the grow home): `open_section` pointed each worker slot's grow home at the slot through its
//! bind-time borrow (`Home::Worker(slot as *mut WorkerSlot)` under `node.workers.iter_mut()`), and every claim reborrows the
//! slot anew (`Open::run_claim`'s `slot`, `claim_on`'s, which clears `state_stale`): accesses the home's pointer is not
//! derived from, so a claim's region grow (`region_grow` -> `slots::grow_partial`, which retires the old buffer and writes
//! the slot's `state_ptr` / `state_bytes` through the home) wrote through a pointer those reborrows had frozen. Plain
//! `cargo test` passes either way; the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_driver --test d1712_worker_grow`. Width 2, three claims
//! per ordinal on the test thread, each ordinal's partial past its share (it grows in its second claim); the kernel reaches
//! its site only through `site_ptr` (a `&mut Site` held across a host call is the kernels' own class, D1705 / D1711). The
//! merge folds each partial's totals into the leader's header, never growing the leader's region: a leader grown by two
//! partials in one section close is D1713, filed apart.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::{next, seek, site};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Call, Claimed, Entry, Grant, Merge, Partial, Result, State, Step, SHARE_FLOOR};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const PER_BLOCK: u64 = 64;
/// A claim's blocks: 2048 rows of 16 bytes, so an ordinal's second claim takes its partial past the 64 KiB share floor.
const CLAIM: u64 = 32;
const CLAIMS: u64 = 6;
const BLOCKS: u64 = CLAIM * CLAIMS;
const NULLS: [u8; 1] = [0];

/// `k_range_grow` (common) over site 0's raw pointer: every row `v` appended at the region's head and `!v` at its tail,
/// the region grown (`region_grow`) whenever the next row does not fit.
unsafe extern "C-unwind" fn k_grow(ctx: *mut Ctx) -> i64 {
    let s = site_ptr(ctx, 0);
    *(*s).keys = (*ctx).claim.lo;
    *(*s).keys.add(1) = (*ctx).claim.hi;
    *(*s).keys_null = 0;
    *(*s).keys_null.add(1) = 0;
    seek(s);
    loop {
        let n = next(site_ptr(ctx, 0));
        if n < 0 {
            return n;
        }
        let s = site_ptr(ctx, 0);
        if n == 0 {
            (*s).state = site::STATE_EOF;
            return DONE;
        }
        let vals = (*(*s).lanes).ptr as *const u64;
        let mut base = own(ctx);
        for i in 0..n as usize {
            let v = *vals.add(i);
            let (head, tail, limit) = {
                let h = grow_hdr(base);
                (h.head, h.tail, h.limit as u64)
            };
            if head + tail + 16 > limit {
                let b = gravity_abi::program::region_grow(ctx, 16, head, tail);
                if b == 0 {
                    return ERROR;
                }
                base = b as usize as *mut u8;
            }
            let h = grow_hdr(base);
            *(base.add(h.head as usize) as *mut u64) = v;
            h.head += 8;
            h.tail += 8;
            *(base.add((h.limit as u64 - h.tail) as usize) as *mut u64) = !v;
            h.count += 1;
            h.sum += v;
        }
        let s = site_ptr(ctx, 0);
        (*s).rows_out += n as u64;
        (*s).cursor = n as u32;
    }
}

/// A partial's `(count, sum, sum of the head records, sum of the complemented tail records)`, read only (`grow_totals`'s).
/// # Safety
/// `p` is a `GrowHdr` region the kernel filled.
unsafe fn totals(p: *const u8) -> [u64; 4] {
    let h = &*(p as *const GrowHdr);
    let len = h.limit as u64;
    let head = (GROW_HDR..h.head).step_by(8).map(|o| *(p.add(o as usize) as *const u64)).sum::<u64>();
    let tail = (len - h.tail..len).step_by(8).map(|o| !*(p.add(o as usize) as *const u64)).sum::<u64>();
    [h.count, h.sum, head, tail]
}

/// Where the leader keeps the four totals: past the header words the driver arms (the limit at offset 8).
const TOTALS: usize = 32;

/// Each partial's totals added into the leader's (its region never grows here).
struct SumMerge;

impl Merge for SumMerge {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        // SAFETY: the partial is a region `k_grow` filled; the leader's region is at least a header long, 8-aligned
        let t = unsafe { totals(partial.bytes.as_ptr()) };
        let out = leader.bytes[TOTALS..TOTALS + 32].as_mut_ptr() as *mut u64;
        for (k, v) in t.iter().enumerate() {
            unsafe { *out.add(k) += v };
        }
        Ok(())
    }
}

/// The emit: the leader's four totals, one row.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let t = consumed(ctx, 0).add(TOTALS) as *const u64;
    let o = (*ctx).out;
    for k in 0..4 {
        *(*o).values.add(k) = *t.add(k);
        *(*o).isnull.add(k) = 0;
    }
    (*o).count = 1;
    if gravity_abi::program::push(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("count", 20, -1, 8, true), OutCol::new("sum", 20, -1, 8, true), OutCol::new("head", 20, -1, 8, true), OutCol::new("tail", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_grow)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit))];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 4, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

#[test]
fn a_workers_region_grow_writes_its_slot_through_the_claims_own_pointer() {
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    for c in 0..CLAIMS {
        let lo = c * CLAIM;
        assert_eq!(open.run_claim((c % 2) as u16, MorselRange::new(lo, lo + CLAIM)).unwrap(), Claimed::Done, "claim {c}");
    }
    for o in 0..2 {
        assert!(open.partial(o).bytes.len() > SHARE_FLOOR as usize, "worker {o}'s partial grew past its share");
    }
    entry.close_section(open, &mut b, &SumMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(8)).unwrap(), Step::Done);
    entry.end(exec, true);
    let rows = BLOCKS * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    assert_eq!(sink.sorted(), vec![vec![rows, sum, sum, sum]], "every row, head and tail intact across the grows");
}
