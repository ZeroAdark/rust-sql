//! The keyed table's posture at its open (fold_entries.rs `estimate_asks_bypass`): from the wide width (8 workers) the
//! estimate asks for the bypass (`T1_EST`, confirmed by evictions in `locate`) once the keys outnumber the table's own
//! slots, whatever the record's width; at or below the slots the table opens keyed. It used to ask past 8 x the slots, so
//! the same keys bypassed with a 24-byte record and stayed keyed with a 16-byte one. Below the wide width it still asks past
//! 8 x the slots: one owner folds every appended row, and keys with locality fold in the table.

use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind, ADAPT_FACTOR, PARTITION_WIDE_WIDTH, T1, T1_BYPASS, T1_EST};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::partial;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::host_abi::{agg_open, AGG_STATE_BYTES};

const SHARE: u32 = 1 << 20;

/// A bound Ctx over one armed share region (the driver's arming, no spill host).
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new() -> Bound {
        let mut own = vec![0u64; SHARE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }
}

/// The open's posture flags and the table's slots for `groups` estimated over `layout`, at the wide width.
fn open(layout: &RecordLayout, groups: u64) -> (u8, u32) {
    open_at(layout, groups, PARTITION_WIDE_WIDTH)
}

/// `open` with the fold's width (`Ctx.width`).
fn open_at(layout: &RecordLayout, groups: u64, width: u16) -> (u8, u32) {
    let mut b = Bound::new();
    let ctx = b.ctx.as_mut_ptr() as *mut Ctx;
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8 + 2];
    unsafe {
        (*ctx).width = width;
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, layout, 0, groups), 0);
        let t = partial(ctx) as *const T1;
        ((*t).flags & (T1_EST | T1_BYPASS), (*t).cap)
    }
}

#[test]
fn the_estimate_asks_the_bypass_past_the_tables_own_slots_at_any_record_width() {
    // a one-word count(DISTINCT)'s pair [x, n] (16 bytes) and a grouped count's record with a count slot (24 bytes)
    const ONE_WORD: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[], false, None);
    const COUNTED: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::Count], false, None);
    for layout in [&ONE_WORD, &COUNTED] {
        let (_, cap) = open(layout, 1 << 40);
        assert!(cap >= 1024, "a capped table: {cap} slots of {} bytes", layout.size());
        let cap = cap as u64;
        assert_eq!(open(layout, cap).0, 0, "{} bytes: as many keys as slots opens keyed", layout.size());
        assert_eq!(open(layout, cap + 1).0, T1_EST, "{} bytes: one key past the slots asks the bypass", layout.size());
        assert_eq!(open(layout, 3 * cap).0, T1_EST, "{} bytes: 3 x the slots (inside the old 8 x) asks the bypass", layout.size());
        assert_eq!(open(layout, cap / 2).0, 0, "{} bytes: half the slots opens keyed", layout.size());
    }
}

#[test]
fn below_the_wide_width_the_estimate_asks_the_bypass_past_8_x_the_slots() {
    const COUNTED: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::Count], false, None);
    let cap = open(&COUNTED, 1 << 40).1 as u64;
    for width in [0, 1, 4, PARTITION_WIDE_WIDTH - 1] {
        assert_eq!(open_at(&COUNTED, cap + 1, width).0, 0, "width {width}: one key past the slots opens keyed");
        assert_eq!(open_at(&COUNTED, ADAPT_FACTOR * cap, width).0, 0, "width {width}: 8 x the slots opens keyed");
        assert_eq!(open_at(&COUNTED, ADAPT_FACTOR * cap + 1, width).0, T1_EST, "width {width}: past 8 x the slots asks the bypass");
    }
    assert_eq!(open_at(&COUNTED, cap + 1, 2 * PARTITION_WIDE_WIDTH).0, T1_EST, "past the wide width: one key past the slots asks");
}

/// The open's partition bits and flags for the IR's radix `bits` (lowerv4 table/radix.rs `bits_for`: 0 = the default; 1..3
/// for a bound of 2..15 tables' worth).
fn open_pbits(layout: &RecordLayout, bits: u64, groups: u64) -> (u8, u8) {
    let mut b = Bound::new();
    let ctx = b.ctx.as_mut_ptr() as *mut Ctx;
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8 + 2];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, layout, bits, groups), 0);
        let t = &*(partial(ctx) as *const T1);
        (t.pbits, t.flags)
    }
}

#[test]
fn a_bound_of_a_few_tables_never_opens_fewer_partitions_than_no_bound() {
    const ONE_WORD: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[], false, None);
    let floor = open_pbits(&ONE_WORD, 0, 0).0;
    assert_eq!(floor, gravity_abi::breaker::PARTITION_DEFAULT_BITS, "no bound: the default");
    for (bits, groups) in [(1, 36_864), (2, 98_304), (3, 196_608)] {
        assert_eq!(open_pbits(&ONE_WORD, bits, groups).0, floor, "{groups} pairs ({bits} IR bits) open at the default's partitions, not under it");
    }
    // only the partitions: with no estimate the open's bypass rule reads the IR's own bits, 1 = the table path
    let (pbits, flags) = open_pbits(&ONE_WORD, 1, 0);
    assert_eq!((pbits, flags & T1_BYPASS), (floor, 0), "IR bits 1 with no estimate: the table path, at the default's partitions");
}
