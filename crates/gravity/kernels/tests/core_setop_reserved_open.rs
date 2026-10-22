//! The setop posture (option c'): a set operation side's keyed table, opened over a region its host reports as an
//! address-space reservation past the share (`SPILL_REGION_COMMIT` with arg 0), is laid out `T1_RESERVED` by either open
//! the lowerer emits: `agg_open`, and table.sized's `agg_open_grown`, which lays the T1 out before `agg_open` runs. The
//! sized open once laid it at the share alone, and `agg_open` then found a live table and never asked, so no sized table
//! ever grew in place (the probes saw setop.reserve fire and setop.grow_in_place never). A host that answers 0, as it does
//! for every other breaker, gets the share's layout as before.

use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind, T1, T1_RESERVED};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::{partial, SPILL_REGION_COMMIT};
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::host_abi::{agg_open, AGG_STATE_BYTES};
use gravity_kernels::table::entries::agg_open_grown;

const SHARE: u32 = 1 << 20;
const RESERVE: u64 = 16 << 20;
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);

/// The driver's answer for a reserved side: the reservation's length. The open only asks; it commits nothing.
unsafe extern "C" fn reserved(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
    if op == gravity_abi::program::SPILL_ARENA_CHUNK {
        return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
    }
    assert_eq!(op, SPILL_REGION_COMMIT, "the open's only host op is the reservation ask");
    assert_eq!(arg, 0, "the open asks for the reservation and commits nothing");
    RESERVE as i64
}

/// The driver's answer for any other breaker's region: no reservation.
unsafe extern "C" fn unreserved(_ctx: *mut Ctx, op: u32, _arg: u64) -> i64 {
    if op == gravity_abi::program::SPILL_ARENA_CHUNK {
        return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
    }
    assert_eq!(op, SPILL_REGION_COMMIT, "the open's only host op is the reservation ask");
    0
}

/// A bound ctx over one region armed at the share, whose bytes run to the reservation's end (the test owns them all).
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new(spill: unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64) -> Bound {
        let mut own = vec![0u64; RESERVE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        program.spill = Some(spill);
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }

    /// The claim's open over a fresh state slot, `agg_open_grown` (table.sized) when `sized`: the laid-out T1.
    fn open(&mut self, sized: bool) -> *mut T1 {
        self.open_bits(sized, 0)
    }

    /// `open` with the IR's radix bits `bits` (0: the breaker's default).
    fn open_bits(&mut self, sized: bool, bits: u64) -> *mut T1 {
        let mut st = vec![0u64; AGG_STATE_BYTES as usize / 8 + 2];
        let ctx = self.ctx.as_mut_ptr() as *mut Ctx;
        let sp = st.as_mut_ptr() as *mut u8;
        let r = unsafe { if sized { agg_open_grown(ctx, sp, &LAYOUT, bits, 1000) } else { agg_open(ctx, sp, &LAYOUT, bits, 1000) } };
        assert_eq!(r, 0, "the open succeeds");
        unsafe { partial(ctx) as *mut T1 }
    }
}

#[test]
fn either_open_lays_a_reserved_side_out_to_grow_in_place() {
    for sized in [false, true] {
        let mut b = Bound::new(reserved);
        let t = b.open(sized);
        unsafe {
            assert_ne!((*t).flags & T1_RESERVED, 0, "sized {sized}: the table is laid out in the reservation");
            assert!((*t).reserved_blocks() > (*t).nblocks, "sized {sized}: its chain words cover blocks past the share's");
            assert!((*t).pool_growth() > 0, "sized {sized}: its pool can grow in place");
            assert_eq!((*t).limit, SHARE, "sized {sized}: laid out and charged at the share");
        }
    }
}

#[test]
fn an_unreserved_region_opens_at_the_share_as_before() {
    for sized in [false, true] {
        let mut b = Bound::new(unreserved);
        let t = b.open(sized);
        unsafe {
            assert_eq!((*t).flags & T1_RESERVED, 0, "sized {sized}: no reservation, no reserved layout");
            assert_eq!((*t).reserved_blocks(), (*t).nblocks, "sized {sized}: the chain words are the share's blocks");
            assert_eq!((*t).pool_growth(), 0, "sized {sized}: nothing to grow into");
        }
    }
}

/// A set operation side's UNION ALL arms fold on into its one region (source/append.rs `continues`): each arm's claim
/// reopens the live table with `agg_open`. After an arm grew the reserved pool in place (`T1::grow_pool` raises `limit`
/// past the share), the reopen keeps the partitions the table was laid with: B3-17's `fold_bits` caps them by the
/// region's `limit`, and a re-cut at the grown limit asked the live table for more (its shape assert, a backend abort
/// in m9_setop_arms' small-work_mem arms).
#[test]
fn a_reopened_grown_side_keeps_its_partitions() {
    for sized in [false, true] {
        let mut b = Bound::new(reserved);
        let t = b.open_bits(sized, 8);
        unsafe {
            assert_ne!((*t).flags & T1_RESERVED, 0, "sized {sized}: the table is laid out in the reservation");
            let pbits = (*t).pbits;
            while T1::grow_pool(t, (*t).pool_growth()) {}
            assert!((*t).limit > 8 * SHARE, "sized {sized}: the pool grew in place well past the share ({})", (*t).limit);
            let again = b.open_bits(false, 8);
            assert_eq!(again, t, "sized {sized}: the arm reopens the same region");
            assert_eq!((*t).pbits, pbits, "sized {sized}: the reopened table keeps its partitions");
            assert_ne!((*t).flags & T1_RESERVED, 0, "sized {sized}: and its reservation");
        }
    }
}
