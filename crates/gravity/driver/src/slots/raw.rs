//! `slots`' unsafe steps (rule R6, the module's raw home): a worker slot's Send / Sync promise, the writes into its
//! bound ctx (rewind, set_claim), its partial read as bytes (state) and the partial's reopen on the worker's thread.
#![allow(unsafe_code)]

use gravity_abi::MorselRange;

use super::WorkerSlot;
use crate::node::Inner;

unsafe impl Send for WorkerSlot {}
unsafe impl Sync for WorkerSlot {}

impl WorkerSlot {
    pub(crate) fn rewind(&mut self, out_rows: u32) {
        let a = self.arena.handle();
        a.reset_to(self.claim_mark);
        self.inner.out.cap = out_rows as i64;
        self.inner.out.count = 0;
        // the window restarts at `pinned`; a slot released to the depot drops its pins
        self.inner.chunk_next = self.inner.pinned;
        unsafe {
            let h = &mut *self.inner.ctx;
            h.arena_cur = gravity_abi::word(self.inner.scratch);
            h.arena_end = gravity_abi::word(self.inner.scratch_end);
            h.resume = gravity_abi::ctx::RESUME_NONE;
        }
    }

    pub(crate) fn set_claim(&mut self, claim: MorselRange) {
        if let Some(b) = self.binding.as_mut() {
            b.claim = claim;
        }
        unsafe { (*self.inner.ctx).claim = claim };
    }

    /// This section's partial (empty for a pipeline that produces nothing).
    pub fn state(&self) -> &[u8] {
        if self.state_ptr.is_null() {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.state_ptr, self.state_bytes as usize) }
    }
}

/// Option c': worker `slot`'s share buffer when its partial is that buffer (a `Budget::Pool` side's reservation lives
/// there), for `grow::region_commit` on the worker's own thread; `None` for a program-constant partial or none.
/// # Safety — `slot` is bound, inside a claim on the thread running it; only its `share_buf` is touched.
pub(crate) unsafe fn share_buf_of<'a>(slot: *mut WorkerSlot) -> Option<&'a mut crate::node::RegionBuf> {
    let share = &mut *core::ptr::addr_of_mut!((*slot).share_buf);
    (!share.is_empty() && (*slot).state_ptr == share.as_mut_ptr()).then_some(share)
}

/// D2894: worker `slot`'s partial grown IN PLACE within its share's reservation (a breaker's floor, or a set operation
/// side's: `grow::grow_in_place`, W first, then pool bytes; `spills`: no wait, the caller spills on a refusal), at most
/// `grow::in_place_cap` under its ceiling `ceil` (0 = none), the limit at offset 8 re-armed, `Partials.own` unchanged.
/// `None` when the partial is no reservation the need fits, or the need passes the ceiling's cap (the caller reopens, or
/// takes its ceiling road), `Some(None)` when the draw is refused (nothing changed).
/// # Safety — as `grow_partial`.
pub(crate) unsafe fn grow_partial_in_place(slot: *mut WorkerSlot, n: &mut Inner, need: u64, head: u64, tail: u64, spills: bool, ceil: u64) -> Option<Option<*mut u8>> {
    let (base, len) = ((*slot).state_ptr, (*slot).state_bytes as u64);
    let buf = share_buf_of(slot)?;
    if !buf.is_reserved() || base.is_null() {
        return None;
    }
    let want = head.saturating_add(tail).saturating_add(need);
    let cap = crate::mem::grow::in_place_cap(buf.reserve() as u64, ceil);
    if ceil != 0 && want > cap {
        return None;
    }
    let new = crate::mem::grow::grown_len(len, need, head, tail)?.min(cap.max(want));
    let worker = n.w_only;
    if !crate::mem::grow::grow_in_place(buf, base, len, new, tail, Some(&mut n.w), n.stmt.as_ref(), worker, spills)? {
        return Some(None);
    }
    gravity_abi::breaker::HashAgg::arm(base, new as u32);
    (*slot).state_bytes = new as u32;
    (*n.partials).own = gravity_abi::word(base);
    n.grew.note(len, new);
    Some(Some(base))
}

/// D254: worker `slot`'s partial reopened on its own thread (`grow::region_grow`): the new buffer drawn from its W, then reserved
/// past it (`chunk_region`), the limit at offset 8 re-armed, `Partials.own` and the slot's state moved to it (what the section's
/// merge reads); the old buffer kept in `retired` until the leader settles it.
/// # Safety — `n` is `slot`'s `inner`, bound, inside a claim on the thread running it; the rest of `slot` is touched by field.
pub(crate) unsafe fn grow_partial(slot: *mut WorkerSlot, n: &mut Inner, need: u64, head: u64, tail: u64) -> crate::Result<*mut u8> {
    let (old, len) = ((*slot).state_ptr, (*slot).state_bytes as u64);
    if old.is_null() {
        return Err(crate::Error::host("gravity: region_grow on a worker with no partial region"));
    }
    // D2894: a partial in its share's reservation (a breaker's floor) grows in place
    match grow_partial_in_place(slot, n, need, head, tail, false, 0) {
        Some(Some(base)) => return Ok(base),
        Some(None) => {
            let pool = &crate::mem::grant::POOL;
            return Err(crate::Error::OutOfMemory { bytes: head.saturating_add(tail).saturating_add(need), held: pool.held(), capacity: pool.capacity(), why: None });
        }
        None => {}
    }
    let new = crate::mem::grow::grown_len(len, need, head, tail).ok_or_else(|| crate::mem::grow::max_region(head.saturating_add(tail).saturating_add(need)))?;
    let stmt = n.stmt.as_ref();
    let (mut buf, base) = crate::mem::grow::reopen(old, len as usize, head as usize, tail as usize, new as usize, &mut n.w, stmt, n.w_only)?;
    if let Some(o) = n.op_owner {
        buf.own_by(o); // LEDGER2 step 5: the reopened partial is its slot owner's, as the one it replaces
    }
    Ok(adopt_partial(slot, n, old, len, new, buf, base))
}

/// `grow_partial` under the worker's ceiling (`grow::region_grow_capped`): its partial reopened at `new` (`grow::capped_len`),
/// the bytes past W the pool's room now (no wait); `None` when the pool has none (nothing changed: the worker spills).
/// # Safety — as `grow_partial`.
pub(crate) unsafe fn grow_partial_to(slot: *mut WorkerSlot, n: &mut Inner, new: u64, head: u64, tail: u64) -> crate::Result<Option<*mut u8>> {
    let (old, len) = ((*slot).state_ptr, (*slot).state_bytes as u64);
    if old.is_null() {
        return Err(crate::Error::host("gravity: region_grow on a worker with no partial region"));
    }
    let stmt = n.stmt.as_ref();
    let Some((mut buf, base)) = crate::mem::grow::reopen_road(old, len as usize, head as usize, tail as usize, new as usize, &mut n.w, stmt, n.w_only, true)? else { return Ok(None) };
    if let Some(o) = n.op_owner {
        buf.own_by(o); // LEDGER2 step 5: as grow_partial's
    }
    Ok(Some(adopt_partial(slot, n, old, len, new, buf, base)))
}

/// The reopened partial `buf` (`base`, `new` bytes) in place of the old one at `old` (`len` bytes): the limit at offset 8
/// re-armed, `Partials.own` and the slot's state moved to it, the old buffer kept in `retired` until the leader settles it.
/// # Safety — as `grow_partial`.
unsafe fn adopt_partial(slot: *mut WorkerSlot, n: &mut Inner, old: *mut u8, len: u64, new: u64, buf: crate::node::RegionBuf, base: *mut u8) -> *mut u8 {
    gravity_abi::breaker::HashAgg::arm(base, new as u32);
    n.mapped_fresh(buf.len() as u64);
    let share = &mut *core::ptr::addr_of_mut!((*slot).share_buf);
    let in_share = !share.is_empty() && old >= share.as_mut_ptr() && old < share.as_mut_ptr().add(share.len());
    let held = if in_share { share } else { &mut *core::ptr::addr_of_mut!((*slot).fixed_buf) };
    let retired = core::mem::replace(held, buf);
    (*core::ptr::addr_of_mut!((*slot).retired)).push(retired); // alloc: Exec
    (*slot).state_ptr = base;
    (*slot).state_bytes = new as u32;
    (*n.partials).own = gravity_abi::word(base);
    n.grew.note(len, new);
    base
}
