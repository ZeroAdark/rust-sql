//! aggtrans's unsafe in exec (R6 home): the hosted aggregate arena's leader-only access, and the plain aggregate's merge of
//! its held states over the record bytes (agg.arena, arena.rs).
#![allow(unsafe_code)]
use core::cell::UnsafeCell;

use gravity_abi::breaker::{table_kind, HashAgg, SlotKind, AGGSTATE_HELD, HASHAGG_HDR, TABLE_AGG};
use gravity_abi::status::raise;
use gravity_driver::{Error, Merge, Partial, Result, State};
use gravity_hosts::v4::m6::AggArena;
use gravity_kernels::ops::agg::trans::hosted::{AggHead, AGG_STATE_BYTES, STATE_AGGNO};
use gravity_parallel::PlainAggPartial;
use types_error::PgResult;

use super::arena::Held;

// SAFETY: the runner calls `merge_into` on the leader, one partial at a time, after the partial's worker finished; the
// arena's worker slots are then no thread's, and its merge slot is the leader's alone.
unsafe impl Sync for Held {}

/// Free the arena's states (each node's shutdown callbacks first) for a re-open (D2289). The caller is the leader
/// re-opening the arena's rerun set before its members run, so no worker and no merge holds the arena.
pub(crate) fn reset_unshared(arena: &UnsafeCell<Box<AggArena>>) {
    // SAFETY: the caller's contract above: the leader alone reaches the arena here.
    unsafe { (*arena.get()).reset() }
}

/// The arena's bytes now (D1421's grant charge). The caller is the leader between pipelines: no worker runs.
pub(crate) fn bytes_unshared(arena: &UnsafeCell<Box<AggArena>>) -> u64 {
    // SAFETY: the caller's contract above: no worker reaches the arena while it reads.
    unsafe { (*arena.get()).bytes() }
}

/// The arena's combine of the held state `from` into `into` (two `AGG_STATE_BYTES` state slots of one aggregate). The
/// caller is the leader's merge (`Held::merge_into`, `Sync` above): no worker reaches the arena.
pub(crate) fn combine_unshared(arena: &UnsafeCell<Box<AggArena>>, aggno: u32, into: *mut u8, from: *const u8) -> PgResult<()> {
    // SAFETY: the caller's contract above; `into` / `from` are the record's and the partial's live state slots.
    unsafe { (*arena.get()).combine(aggno, into, from) }
}

/// D798: the combine of two held states of one aggregate (`into`, `from`: `AGG_STATE_BYTES` slots, the aggregate's number
/// read from `into`'s) in worker `ordinal`'s slot through the arena's `AggHead.combine` (`from` left zeroed); `Err(())` with
/// PG's error stashed. The caller is the owner phase on the leader after the workers finished (as `merge_into`).
#[allow(dead_code)] // its reader is `Held::combiner` (owner.rs fold_pair, M6-FOLD D798), stacked on this row
pub(crate) fn combine_held(arena: &UnsafeCell<Box<AggArena>>, ordinal: u16, into: *mut u8, from: *mut u8) -> core::result::Result<(), ()> {
    // SAFETY: the caller's contract above; the head is the live arena's, `into` / `from` are two state slots of one
    // aggregate.
    unsafe {
        let head: *mut AggHead = (*arena.get()).handle() as *mut AggHead;
        let aggno = (into.add(STATE_AGGNO) as *const u32).read_unaligned();
        ((*head).combine)(head, ordinal, aggno, into, from)
    }
}

/// The plain aggregate's merge of one partial: each hosted state both the leader's record and the partial's hold is
/// combined into the leader's by `combine` (the arena's combinefn), the partial's copy then holds it no more, and the
/// runner's plain fold takes the rest (`Rec::fold` moves a lone held state and never sees two). The region is the
/// record's: fold/hosted.rs sizes it at lowering (D792) and refuses a smaller one as a Bug; D806 was a merge that
/// declined a short region and left two held states to `Rec::fold`.
/// D1404: a held state the leader's record takes whole (its own before the first merge, a partial's lone one moved by the
/// fold) lives in a worker slot's context, and a later combine grows it through the merge slot's (string_agg_combine
/// appends into its first state: a foreign dedicated chunk, a panic). PG never meets it (a parallel Agg serializes
/// internal states); here each such state is rehomed, combined from an empty state into the merge slot's context
/// (PG's combine with a NULL first state; a strict combinefn copies a by-reference state, an internal state's combinefn
/// is never strict).
pub(crate) fn merge_hosted(leader: &mut State<'_>, partial: Partial<'_>, combine: &mut dyn FnMut(u32, *mut u8, *const u8) -> PgResult<()>, homed: &core::cell::Cell<bool>) -> Result<()> {
    // SAFETY: a plain aggregate's partial and leader regions, each a record table of one (`single_record`).
    unsafe {
        let src = single_record(partial.bytes);
        if !homed.get() {
            if let Some(dst) = single_record(leader.bytes) {
                let layout = HashAgg::view(leader.bytes.as_ptr()).layout;
                for j in 0..layout.nslots() {
                    rehome(combine, dst, layout.slot_kind(j), layout.slot_off(j) as usize)?;
                }
                homed.set(true);
            }
        }
        let Some(src) = src else { return PlainAggPartial.merge_into(leader, partial) };
        let layout = HashAgg::view(partial.bytes.as_ptr()).layout;
        let held = |r: *mut u8, j: usize| layout.slot_kind(j) == SlotKind::AggState && *r.add(layout.slot_off(j) as usize + AGGSTATE_HELD) != 0;
        let before = single_record(leader.bytes);
        let moved: Vec<usize> = (0..layout.nslots()).filter(|&j| held(src, j) && before.is_none_or(|d| !held(d, j))).collect(); // alloc: Exec
        match before {
            Some(dst) if (0..layout.nslots()).any(|j| held(src, j) && held(dst, j)) => {
                let need = HASHAGG_HDR + layout.size() as usize;
                debug_assert!(partial.bytes.len() >= need && leader.bytes.len() >= need, "a hosted record outside its plain region (D792, D806)");
                debug_assert!(HashAgg::view(leader.bytes.as_ptr()).layout == layout, "a plain partial of another layout");
                let mut copy = vec![0u64; partial.bytes.len().max(need).div_ceil(8)]; // alloc: Exec
                let whole = core::slice::from_raw_parts_mut(copy.as_mut_ptr().cast::<u8>(), copy.len() * 8);
                whole[..partial.bytes.len()].copy_from_slice(partial.bytes);
                let at = src.offset_from(partial.bytes.as_ptr()) as usize;
                for j in (0..layout.nslots()).filter(|&j| held(src, j) && held(dst, j)) {
                    let off = layout.slot_off(j) as usize;
                    let aggno = (src.add(off + STATE_AGGNO) as *const u32).read_unaligned();
                    if let Err(e) = combine(aggno, dst.add(off), src.add(off)) {
                        gravity_fn::stash_error(e);
                        return Err(Error::Raised(raise::bit(raise::HOST)));
                    }
                    whole[at + off + AGGSTATE_HELD] = 0;
                }
                PlainAggPartial.merge_into(leader, Partial { bytes: &whole[..partial.bytes.len()], ..partial })?;
            }
            _ => PlainAggPartial.merge_into(leader, partial)?,
        }
        if let Some(dst) = single_record(leader.bytes) {
            for &j in &moved {
                rehome(combine, dst, layout.slot_kind(j), layout.slot_off(j) as usize)?;
            }
            homed.set(true);
        }
        Ok(())
    }
}

/// D1404: the held state at `rec + off` (an `AggState` slot) combined from an empty state into the merge slot's context and
/// written back in place; any other slot kind, or a state not held, is left as it is.
unsafe fn rehome(combine: &mut dyn FnMut(u32, *mut u8, *const u8) -> PgResult<()>, rec: *mut u8, kind: SlotKind, off: usize) -> Result<()> {
    if kind != SlotKind::AggState || *rec.add(off + AGGSTATE_HELD) == 0 {
        return Ok(());
    }
    let mut fresh = [0u64; AGG_STATE_BYTES as usize / 8];
    let aggno = (rec.add(off + STATE_AGGNO) as *const u32).read_unaligned();
    if let Err(e) = combine(aggno, fresh.as_mut_ptr().cast(), rec.add(off)) {
        gravity_fn::stash_error(e);
        return Err(Error::Raised(raise::bit(raise::HOST)));
    }
    core::ptr::copy_nonoverlapping(fresh.as_ptr().cast::<u8>(), rec.add(off), AGG_STATE_BYTES as usize);
    Ok(())
}

/// The one used record of a plain aggregate's region (a record table of one), if `bytes` holds one.
unsafe fn single_record(bytes: &[u8]) -> Option<*mut u8> {
    if bytes.len() < HASHAGG_HDR || table_kind(bytes.as_ptr()) != TABLE_AGG {
        return None;
    }
    let t = HashAgg::view(bytes.as_ptr());
    (t.cap == 1 && t.len == 1).then(|| t.record(0).as_ptr())
}
