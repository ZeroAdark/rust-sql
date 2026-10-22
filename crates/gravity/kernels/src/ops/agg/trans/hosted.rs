//! The hosted aggregate bracket (face/aggcall.rs, the `agg.arena` protocol): the transition and final calls of an
//! aggregate PG runs through fmgr (nodeAgg.c `advance_transition_function` / `finalize_aggregate`), reached through the
//! statement's aggregate arena (hosts ops/agg/trans/hosted.rs `AggArena`, prepared by exec ops/agg/trans/arena.rs)
//! behind the handle word `AGG_W_HEAD`. The entries are written in the raw child as `contract::host` functions under the
//! one landing pad (D1520's `m6_agg_final_direct` under `m6_agg_final`'s), stashing PG's error and returning `ERROR`,
//! and re-exported here. Each call names its worker's ordinal (the Ctx's): a parallel arena keeps a slot per ordinal
//! (its own calls and aggcontext), the partial states combined by exec's merge.
mod raw;

pub use raw::{at_home, m6_agg_combine, m6_agg_final, m6_agg_final_direct, m6_agg_fold, m6_agg_home, m6_agg_home_open, m6_agg_trans, owner_combine, AggHead};

use gravity_abi::breaker::{RecordLayout, SlotKind};
use gravity_fn::PgError;

/// The `agg.arena` protocol's words: the arena's handle.
pub const AGG_WORDS: u16 = 1;
pub const AGG_W_HEAD: usize = 0;

/// aggtrans T6k: the agg.arena Proto argument's bit (lowerv4 fold/hosted.rs `arena_word`, beside D1596's bit 62) for a group
/// fed in the order of the Sort PostgreSQL put under its Agg: exec's agg.arena calls the host's `ordered_fed`, so each
/// aggpresorted ORDER BY aggregate advances per row as its rows come (nodeAgg with `aggsortrequired` false: no run kept,
/// no sort per group at the final).
pub const AGG_ORDERED_FED: u64 = 1 << 61;
/// D1596: the agg.arena Proto argument's bit (lowerv4 fold/hosted.rs `arena_word`) for a plain group whose aggpresorted
/// DISTINCTs fold per input row in its Sort's order: exec's agg.arena calls the host's `presorted_fed` (hosts
/// hosted_presorted.rs). Its home is the agg family's, beside `AGG_ORDERED_FED` (was group's regroup.rs `PRESORTED`, which
/// only the agg family read; rule RA).
pub const AGG_PRESORTED_FED: u64 = 1 << 62;
/// The agg.arena Proto argument's bit for a streamed sorted group (one open group at a time, handed on when the next
/// group's first row comes): exec's agg.arena calls the host's `streamed`, so the first transition after a group's finals
/// resets the aggregate context, as nodeAgg.c agg_retrieve_direct's ReScanExprContext of each aggcontext at a new group
/// (each state's shutdown callbacks run: an ordered-set aggregate's tuplesort_end, ordered_set_shutdown).
pub const AGG_STREAMED: u64 = 1 << 60;

/// One aggregate's state slot in a record, the carry's spill or a state area (`AGG_STATE_BYTES` at 8-byte alignment): the
/// transition value, whether it is NULL, PG's `noTransValue` (a strict transfn's first input becomes the state), and
/// whether the slot holds a state at all, and the aggregate's number (`u32`, the merge's key). A zeroed slot is a group
/// not yet initialized: the first call initializes it (`initialize_aggregate`: the initcond, copied per group when by
/// reference). V4R-375: a keyed record's slot tagged `STATE_HOME` holds no state: its first word is the address of its
/// group's one home state (`m6_agg_home`), which every call reads through (`at_home`).
pub const STATE_VALUE: usize = 0;
pub const STATE_NULL: usize = 8;
pub const STATE_NOTRANS: usize = 9;
pub const STATE_INIT: usize = 10;
pub const STATE_HOME: usize = 11;
pub const STATE_AGGNO: usize = 12;
pub const AGG_STATE_BYTES: u32 = 16;

/// D1520: a direct argument the host bound at EXECUTE (a constant), one evaluated over the group's row, a NULL.
pub const DIRECT_BOUND: u8 = 0;
pub const DIRECT_VALUE: u8 = 1;
pub const DIRECT_NULL: u8 = 2;

pub const AGG_MAGIC: u64 = 0xa66a_7e4a_a6e0_5ac1;

/// V4R-375: the slot of a keyed hosted record that carries its arena's handle: the `MaxI64` right after the last `AggState`
/// (lowerv4 fold/hosted.rs's keyed layout); `None` for a layout with no hosted state.
pub fn handle_slot(l: &RecordLayout) -> Option<usize> {
    let h = l.slot_kinds().iter().rposition(|&k| k == SlotKind::AggState)? + 1;
    (l.slot_kinds().get(h) == Some(&SlotKind::MaxI64)).then_some(h)
}

#[cold]
fn stale() -> i64 {
    crate::ops::agg::trans::entries::failed(Box::new(PgError::error("gravity: a hosted aggregate call over no live arena"))) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::Rec;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    // V4R-427: counts `combine_keep` (the combinefn's door); the other entries are never reached by `owner_combine`
    static KEEPS: AtomicU64 = AtomicU64::new(0);
    unsafe fn keep(_h: *mut AggHead, _o: u16, _a: u32, _i: *mut u8, _f: *const u8) -> Result<(), ()> {
        KEEPS.fetch_add(1, Relaxed);
        Ok(())
    }
    unsafe fn trans(_h: *mut AggHead, _o: u16, _a: u32, _s: *mut u8, _x: *const u64, _n: *const u8) -> Result<(), ()> { unreachable!() }
    unsafe fn fin(_h: *mut AggHead, _o: u16, _a: u32, _s: *mut u8, _out: *mut u64) -> Result<(), ()> { unreachable!() }
    unsafe fn fold(_h: *mut AggHead, _o: u16, _a: u32, _s: *mut u8) -> Result<(), ()> { unreachable!() }
    unsafe fn comb(_h: *mut AggHead, _o: u16, _a: u32, _i: *mut u8, _f: *mut u8) -> Result<(), ()> { unreachable!() }
    unsafe fn home(_h: *mut AggHead, _c: *mut gravity_abi::Ctx, _k: *const u64, _n: usize, _m: usize) -> *mut u8 { unreachable!() }
    unsafe fn clear(_h: *mut AggHead) { unreachable!() }
    unsafe fn gather(_h: *mut AggHead, _s: *mut u8) -> *mut u8 { unreachable!() }

    /// A keyed hosted record (`[AggState, MaxI64]`, the handle in the `MaxI64`) whose state slot is tagged on `on` (a
    /// home) or, with `None`, holds a state of its own.
    fn record(layout: &RecordLayout, head: *mut AggHead, on: Option<&[u64; 2]>) -> Vec<u64> {
        let mut r = vec![0u64; (layout.size as usize).div_ceil(8)]; // alloc: Cold
        let p = r.as_mut_ptr() as *mut u8;
        // SAFETY: both slots lie inside the record's `size` bytes.
        unsafe {
            (p.add(layout.slot_off(1) as usize) as *mut u64).write_unaligned(head as u64);
            let s = p.add(layout.slot_off(0) as usize);
            if let Some(h) = on {
                (s as *mut u64).write_unaligned(h.as_ptr() as u64);
                s.add(STATE_HOME).write(1);
            }
            s.add(STATE_INIT).write(1);
        }
        r
    }

    fn head() -> *mut AggHead {
        let h = AggHead { magic: AGG_MAGIC, trans, final_: fin, fold, combine: comb, combine_keep: keep, home, homes_clear: clear, direct: 0, given: 0, stable: 0, wide: 1, gather };
        Box::into_raw(Box::new(h)) // alloc: Cold
    }

    /// V4R-427 (D1313): a hashed group's partials all point at its one home, so the owner fold takes the tag and never
    /// calls the combinefn: a group whose aggregate has none (a user or ordered-set one, a float8 state) folds exactly.
    #[test]
    fn a_keyed_groups_partials_on_one_home_never_reach_the_combinefn() {
        let layout = RecordLayout::new(&[], &[SlotKind::AggState, SlotKind::MaxI64]);
        let (h, (a, b)) = (head(), ([0u64; 2], [0u64; 2]));
        let (mut r1, mut r2, mut r3) = (record(&layout, h, Some(&a)), record(&layout, h, Some(&a)), record(&layout, h, Some(&b)));
        let mut into = [0u64; 2];
        let off = layout.slot_off(0) as usize;
        let slot = |r: &mut Vec<u64>| unsafe { (r.as_mut_ptr() as *mut u8).add(off) as *const u8 };
        let before = KEEPS.load(Relaxed);
        // SAFETY: each record is `layout`'s, its handle a live `AggHead`; `into` is an `AGG_STATE_BYTES` slot.
        unsafe {
            let src = |r: &mut Vec<u64>| Rec::at(r.as_mut_ptr() as *mut u8, &layout);
            assert!(owner_combine(&src(&mut r1), 0, into.as_mut_ptr() as *mut u8, slot(&mut r1)), "an empty slot takes the tag");
            assert_eq!(into[0], a.as_ptr() as u64);
            assert!(owner_combine(&src(&mut r2), 0, into.as_mut_ptr() as *mut u8, slot(&mut r2)), "a second partial on the same home");
            assert!(!owner_combine(&src(&mut r3), 0, into.as_mut_ptr() as *mut u8, slot(&mut r3)), "another home beside it: a named error");
        }
        assert_eq!(KEEPS.load(Relaxed), before, "no combinefn call");
        // SAFETY: made by Box::into_raw in `head`.
        drop(unsafe { Box::from_raw(h) });
    }

    /// V4R-427: a keyed hosted source off its home is a Bug the debug build asserts (release still calls the combinefn,
    /// which raises by name without one: hosts m6_agg `combine_in`).
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "V4R-427"))]
    fn a_keyed_source_off_its_home_is_asserted() {
        let layout = RecordLayout::new(&[], &[SlotKind::AggState, SlotKind::MaxI64]);
        let h = head();
        let mut r = record(&layout, h, None);
        let mut into = [0u64; 2];
        let before = KEEPS.load(Relaxed);
        // SAFETY: as above.
        unsafe {
            let from = (r.as_mut_ptr() as *mut u8).add(layout.slot_off(0) as usize) as *const u8;
            assert!(owner_combine(&Rec::at(r.as_mut_ptr() as *mut u8, &layout), 0, into.as_mut_ptr() as *mut u8, from));
        }
        assert_eq!(KEEPS.load(Relaxed), before + 1, "release: the combinefn's door, once");
    }
}
