//! D4631: a parked worker set kept its slots' per-statement lists at the capacity its statement grew them to (the buffers
//! a grown partial replaced, the arena's chunk and spill lists, the pin lists, the run readers): emptied at the statement's
//! end and kept for the next one, but grown on the statement's threads and charged nowhere. The idle release of another
//! backend dropped the set (`SlotDepot::trim_for`) and freed those blocks on that backend's thread, below its account's
//! zero, and `ledger_floor` credited them to UNBOUND (or its own balance absorbed them). Now the park charges the lists'
//! blocks to the slot depot's holder at the put (`Parked::held`, declared first): a set whose statement grew its
//! lists frees no more uncovered on the trimming backend than a set of the same slots whose statement grew none (both
//! free the slots' own build there, a separate step). One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::{next, seek, site};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::{Ctx, MorselRange};
use gravity_driver::{Call, Claimed, Entry, Grant, Merge, Partial, Result, State, Step, DEPOT, SHARE_FLOOR};
use gravity_program::{Arena, Code, End, Layout as ProgLayout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};
use memledger::Acct;

/// Door 1, as the server's allocator wrapper counts it (init_small oom.rs).
struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static ALLOC: Door1 = Door1;

const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 4;
const PER_BLOCK: u64 = 64;
/// A grown statement's claim: 256 blocks, 16,384 rows of 16 bytes (256 KiB), so each ordinal's partial grows from the share
/// floor several times over its four claims (each grow retires the buffer it replaced onto the slot's list).
const GROWN: u64 = 256;
/// A flat statement's claim: 8 blocks, 512 rows (8 KiB): every partial fits its share floor, no list grows.
const FLAT: u64 = 8;
const CLAIMS: u64 = 4 * WIDTH as u64;
const SALT: u64 = 0x4631_1157;

/// Every row `v` appended at the region's head and `!v` at its tail, the region grown (`region_grow`) whenever the next row
/// does not fit.
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

/// Where the leader keeps the row count: past the header words the driver arms (the limit at offset 8).
const TOTALS: usize = 32;

/// Each partial's row count added into the leader's (its region never grows here).
struct CountRows;

impl Merge for CountRows {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        // SAFETY: the partial is a region `k_grow` filled (a `GrowHdr` first); the leader's region is a header long, 8-aligned
        let n = unsafe { (*(partial.bytes.as_ptr() as *const GrowHdr)).count };
        let out = leader.bytes[TOTALS..TOTALS + 8].as_mut_ptr() as *mut u64;
        // SAFETY: in the leader's region, 8-aligned
        unsafe { *out += n };
        Ok(())
    }
}

/// The emit: the leader's row count, one row.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let t = consumed(ctx, 0).add(TOTALS) as *const u64;
    let o = (*ctx).out;
    *(*o).values = *t;
    *(*o).isnull = 0;
    (*o).count = 1;
    if gravity_abi::program::push(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("count", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_grow)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit))];
    Program::with_pipelines(ProgLayout { nsites: 1, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn bytes(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
}

/// One salted parallel statement on backend `a` with claims of `claim` blocks, its set parked at the end; then backend
/// `c`'s trim drops the set. -> (C's account after the trim less before, the floor's folds and bytes in the trim).
fn park_then_trim(a: u32, c: u32, claim: u64) -> (i64, u64, i64) {
    let blocks = claim * CLAIMS;
    let table = RangeTable::new(blocks, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(program());
    let flag = AtomicU8::new(0);
    memledger::bind_backend(a, 9000 + a);
    let mut entry = Entry::new(2);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    exec.set_slot_salt(Some(SALT));
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
    for k in 0..CLAIMS {
        let lo = k * claim;
        assert_eq!(open.run_claim((k % WIDTH as u64) as u16, MorselRange::new(lo, lo + claim)).unwrap(), Claimed::Done, "claim {k}");
    }
    for o in 0..WIDTH {
        let grew = open.partial(o).bytes.len() > 4 * SHARE_FLOOR as usize;
        assert_eq!(grew, claim == GROWN, "worker {o}'s partial grew past its share several times only in the grown statement");
    }
    entry.close_section(open, &mut b, &CountRows, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(8)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted(), vec![vec![blocks * PER_BLOCK]], "every row counted");
    assert_eq!(DEPOT.stats().entries, 1, "the set parks");
    drop(entry);
    memledger::unbind_backend();

    memledger::bind_backend(c, 9000 + c);
    let acct = Acct::backend(c);
    let c0 = bytes(acct);
    let (f0, b0) = memledger::floored();
    assert!(DEPOT.trim_for(usize::MAX) > 0, "the trim drops the parked set");
    assert_eq!(DEPOT.stats().entries, 0);
    let c1 = bytes(acct);
    let (f1, b1) = memledger::floored();
    memledger::unbind_backend();
    (c1 - c0, f1 - f0, b1 - b0)
}

#[test]
fn a_parked_sets_lists_leave_with_their_statement_and_its_drop_on_another_backend_frees_none_of_them() {
    std::thread::spawn(|| {
        DEPOT.clear();
        memledger::set_floor(true);
        // the same program at the same width builds the same slots: what a flat statement's set frees on C at the trim is
        // the slots' own build, which a grown statement's set frees there too
        let (flat, ff, fb) = park_then_trim(4631, 4633, FLAT);
        let (grown, gf, gb) = park_then_trim(4632, 4634, GROWN);
        memledger::set_floor(false);
        assert_eq!((ff, fb, gf, gb), (0, 0, 0, 0), "no fold of either trim floored its backend's account (folds, bytes)");
        assert_eq!(grown, flat, "the grown statement's lists freed nothing on the trimming backend past the slots' own build");
    })
    .join()
    .unwrap();
}
