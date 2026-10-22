//! `arena`'s unsafe half (rule R6, the module's raw home): its host entries and the raw-pointer steps they take,
//! moved verbatim from arena.rs (private ones pub(super)); it imports them back.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

gravity_abi::host! {
    /// `Program.arena_grow`: the bump window moved to a pool chunk of ≥ `bytes` (kept across executions); its size, or 0 when
    /// `chunk_region` refuses the chunk's bytes (the worker's W, else the statement's G, else the past-G road: D642, never a
    /// chunk count).
    /// # Safety — `ctx` is bound.
    pub unsafe fn grow(ctx: *mut Ctx, bytes: u64) -> i64 {
        guarded((*(*ctx).program.ptr()).env.ptr(), || grow_in(ctx, bytes), |_| 0)
    }
}

/// A region of at least `bytes` for a runtime object of the execution `ctx` runs (a spool's records or copies), held until
/// the object drops it: the worker's W, else the statement's G (`grow`'s road; a worker's claim purse, D4043). C10 (D4168,
/// SPILL-DEBT's `SpoolPastGrant` paid): past W and G a session with a temp-file road (`spill::file_road`) gets `None` at
/// once (`chunk_region_within`) and the caller's road maps the spool's temp file (`object_map`, exec ops/material `Road`),
/// as PostgreSQL's tuplestore writes its rows out past work_mem. The pool's room stays for the statement's breakers: a
/// grouping-set record spool drew the 64 MiB pool past G to its last page and its own breaker region then failed out of
/// memory. Without a file road (a rig), past G at its size counted as grown (D225), never a refusal while the pool has room;
/// `None` is the pool's out of memory.
/// # Safety — `ctx` is bound.
pub unsafe fn object_region(ctx: *mut Ctx, bytes: usize) -> Option<crate::RegionBuf> {
    let n = inner_of(ctx);
    // SAFETY: as `Inner::stmt` (null on a worker slot).
    let stmt = unsafe { n.stmt.as_ref() };
    let mut r = if crate::mem::spill::file_road() {
        crate::node::chunk_region_within(bytes, Some(&mut n.w), stmt, n.w_only)
    } else {
        crate::node::chunk_region(bytes, Some(&mut n.w), stmt, n.w_only)
    };
    // LEDGER2 step 5: the running pipeline's slot owner holds it
    if let (Some(r), Some(o)) = (r.as_mut(), n.op_owner) {
        r.own_by(o);
    }
    r
}

/// D3236 (the re-audit's R2 spools row, F6): a runtime object's road once the pool refused its `object_region` (exec
/// ops/material/raw.rs `Road`, a spool past the pool): `bytes` of a temp file mapped shared (D491's `map_temp`: no pool
/// charge, its bytes charged against temp_file_limit and 53400 past it, unlinked once mapped, written back by the kernel
/// under memory pressure as PostgreSQL's tuplestore file is), zeroed, counted as mapped past the pool. The statement's
/// noted refusal is cleared, as a served draw's is (D2845: a later scratch refusal is not named after it). `Ok(None)`: the
/// session has no file road, the caller's out of memory stands.
/// # Safety — `ctx` is bound, or null (no statement to clear: a rig).
pub unsafe fn object_map(ctx: *mut Ctx, bytes: usize) -> crate::Result<Option<crate::mem::spill::TempMap>> {
    let Some(mut m) = crate::mem::spill::map_temp(bytes)? else { return Ok(None) };
    crate::mem::spill::note_mapped_past(m.len());
    if !ctx.is_null() {
        let n = inner_of(ctx);
        // LEDGER2 step 5: the running pipeline's slot owner holds it
        if let Some(o) = n.op_owner {
            m.own_by(o);
        }
        // SAFETY: as `Inner::stmt` (null on a worker slot).
        if let Some(s) = unsafe { n.stmt.as_ref() } {
            s.note_refused(0);
        }
    }
    Ok(Some(m))
}

/// D1378: `bytes` more of a runtime object's heap memory for the execution `ctx` runs (a recursive UNION's
/// seen set's doubling, exec setop/generation.rs) held as an overdraft (`GrantPool::overdraw`, `Overdraw::Seen`): never
/// refused (PostgreSQL's table has no spill road), counted past G on the statement (a worker slot's has none), back when the
/// hold drops.
/// # Safety — `ctx` is bound.
pub unsafe fn object_overdraw(ctx: *mut Ctx, bytes: u64) -> crate::mem::grant::Held {
    let n = inner_of(ctx);
    // SAFETY: as `Inner::stmt` (null on a worker slot).
    let stmt = unsafe { n.stmt.as_ref() };
    crate::mem::grant::POOL.overdraw(bytes, stmt, crate::mem::grant::Overdraw::Seen)
}

/// # Safety — As `grow`.
#[inline(always)]
pub(super) unsafe fn grow_in(ctx: *mut Ctx, bytes: u64) -> i64 {
    let n = inner_of(ctx);
    let want = (bytes as usize).max(CHUNK);
    let idx = match (n.chunk_next..n.chunks.len()).find(|&i| n.chunks[i].len() >= want) {
        Some(i) => i,
        None => {
            // SAFETY: as `Inner::stmt` (null on a worker slot).
            let stmt = unsafe { n.stmt.as_ref() };
            let Some(chunk) = crate::node::chunk_region(want, Some(&mut n.w), stmt, n.w_only) else { return grow_mapped(ctx, want) };
            n.mapped_fresh(chunk.len() as u64);
            n.chunks.push(chunk); // alloc: Exec
            n.chunks.len() - 1
        }
    };
    n.chunks.swap(n.chunk_next, idx);
    let c = &mut n.chunks[n.chunk_next];
    n.chunk_next += 1;
    (*ctx).arena_cur = gravity_abi::word(c.as_mut_ptr());
    (*ctx).arena_end = gravity_abi::word(c.as_mut_ptr().add(c.len()));
    c.len() as i64
}

/// D3035 (its road for a kernel's pins, `pin_host`): `bytes` pinned past the grant pool, carved from a temp file mapped shared (`map_temp`)
/// when the pool refused the pin's chunk: a one-pass group table whose statement-lived images (a kept text extreme per
/// group, 100 M groups) outgrow the statement's pool cap spills them as PostgreSQL's temp files do instead of failing the
/// statement with "the statement's pinned pool is full". The pins carve one mapping at a time (`Inner.pin_map`, each
/// twice the last); every mapping is held with the serial owners' to the statement's end or a reset (`owner_maps`) and is
/// never reused by a rewind, so a rerun set's pins never alias. Fresh file pages read zero. Null when the session has no
/// file road (the refusal as before) or the file is refused, its error held for the call (D4305: 53400 past temp_file_limit).
/// # Safety — As `pin`.
#[cold]
unsafe fn pin_mapped(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    const ALIGN: usize = 16;
    let n = inner_of(ctx);
    let need = bytes.div_ceil(ALIGN) * ALIGN;
    let (next, end) = n.pin_map;
    if next == 0 || end - next < need {
        // each mapping twice the last (the first at the refused chunk's size): a pin per group maps O(log n) files
        let last = if next == 0 { 0 } else { n.owner_maps.last().map_or(0, |m| m.len()) };
        let len = need.max(CHUNK).max(last * 2);
        let m = match crate::mem::spill::map_temp(len) {
            Ok(Some(m)) => m,
            Ok(None) => return core::ptr::null_mut(),
            // D4305: the file's own error (53400 past temp_file_limit, ENOSPC) is the kind 2's that follows
            Err(e) => {
                n.err.get_or_insert(e);
                return core::ptr::null_mut();
            }
        };
        let p = m.as_mut_ptr() as usize;
        n.owner_maps.push(m); // alloc: Cold
        crate::mem::spill::note_mapped_past(len);
        n.pin_map = (p, p + len);
    }
    let p = n.pin_map.0;
    n.pin_map.0 += need;
    p as *mut u8
}

/// D3035: the bump window moved to `want` bytes of a temp file mapped shared (`map_temp`, D491's road) when the grant pool
/// refused the chunk: a statement held at its pool cap (`pgrust.gravity_statement_pool_fraction`) whose emit gathers one
/// pointer per group (100 M groups: an 800 MB array) spills it as PostgreSQL's temp files do instead of failing the run and
/// falling through to the row engine. The mapping is no pool charge and outlives every rewind of the window: it is held with
/// the serial owners' mappings to the statement's end (`drop_result_spills`, a reset). 0 when the session has no file road
/// (the refusal as before) or the file is refused, its error held for the call (D4305).
/// # Safety — As `grow`.
#[cold]
unsafe fn grow_mapped(ctx: *mut Ctx, want: usize) -> i64 {
    let m = match crate::mem::spill::map_temp(want) {
        Ok(Some(m)) => m,
        Ok(None) => return 0,
        // D4305: as `pin_mapped`'s (node/raw.rs `settle_call` raises a held error before a kind 2)
        Err(e) => {
            inner_of(ctx).err.get_or_insert(e);
            return 0;
        }
    };
    let n = inner_of(ctx);
    let p = m.as_mut_ptr();
    n.owner_maps.push(m); // alloc: Cold
    crate::mem::spill::note_mapped_past(want);
    (*ctx).arena_cur = gravity_abi::word(p);
    (*ctx).arena_end = gravity_abi::word(p.add(want));
    want as i64
}

/// D641: a region of `bytes` for the hosted result region's spill (lowerv4 hir.rs), drawn as `grow` draws (W, else G, else
/// D225's reserve past both), held by this owner until the statement's end (`spill_epoch` moves, then its pages are
/// MADV_FREE'd and it parks in the depot, D2687); null when the grant pool refuses.
/// # Safety — `ctx` is bound.
pub unsafe fn spill_region(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    guarded((*(*ctx).program.ptr()).env.ptr(), || spill_in(ctx, bytes, true, false), |_| core::ptr::null_mut())
}

/// D4425: `spill_region` whose draw past W and G takes the pool's room as it is now (`chunk_region_now`: no wait, no
/// refusal kept on the statement), null when it has none: a hash join reload's region, whose road past the pool is the
/// statement's mapped temp file (`spill_host::v4::take_reload`).
/// # Safety — As `spill_region`.
pub(crate) unsafe fn spill_region_now(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    guarded((*(*ctx).program.ptr()).env.ptr(), || spill_in(ctx, bytes, true, true), |_| core::ptr::null_mut())
}

/// D3644 (b): `spill_region` not zeroed (`chunk_region_unzeroed`: no memset of reused pages; huge pages asked for as
/// `uninit_region` asks), for a block its taker writes before it reads (a group sort's arrays).
/// # Safety — As `spill_region`.
pub(crate) unsafe fn spill_region_unzeroed(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    guarded((*(*ctx).program.ptr()).env.ptr(), || spill_in(ctx, bytes, false, false), |_| core::ptr::null_mut())
}

/// D4373: `spill_region_unzeroed` within the grant: W, else G (`region::chunk_region_within`), else with `pool_now` the
/// pool's room now; null past them, no refusal kept (the caller's road past them is the statement's mapped temp file).
/// # Safety — As `spill_region`.
pub(crate) unsafe fn spill_region_within(ctx: *mut Ctx, bytes: usize, pool_now: bool) -> *mut u8 {
    guarded((*(*ctx).program.ptr()).env.ptr(), || {
        let n = inner_of(ctx);
        // SAFETY: as `Inner::stmt` (null on a worker slot).
        let stmt = unsafe { n.stmt.as_ref() };
        let chunk = match crate::mem::region::chunk_region_within(bytes, Some(&mut n.w), stmt, n.w_only) {
            Some(c) => Some(c),
            // `pool_now`: past W and G the pool's room as it is now (no wait, no refusal kept), counted as grown
            None if pool_now => crate::mem::region::chunk_region_unzeroed(bytes, Some(&mut n.w), stmt, n.w_only, true),
            None => None,
        };
        let Some(mut chunk) = chunk else { return core::ptr::null_mut() };
        n.mapped_fresh(chunk.len() as u64);
        // LEDGER2 step 5: the running pipeline's slot owner holds it
        if let Some(o) = n.op_owner {
            chunk.own_by(o);
        }
        let p = chunk.as_mut_ptr();
        n.spills.push(chunk); // alloc: Exec
        p
    }, |_| core::ptr::null_mut())
}

/// Ladder step 1.1 (`gate_now`): `spill_region` (`zeroed`) or `spill_region_unzeroed` for a draw whose caller maps the
/// statement's temp file when this is null (`spill_host::v4::take_road` with a file road): past W and G the gate's
/// `Spills` asker (`node::chunk_region_spills`). With the knob off, their road exactly.
/// # Safety — As `spill_region`.
pub(crate) unsafe fn spill_region_spills(ctx: *mut Ctx, bytes: usize, zeroed: bool) -> *mut u8 {
    guarded((*(*ctx).program.ptr()).env.ptr(), || spill_in_as(ctx, bytes, zeroed, false, true), |_| core::ptr::null_mut())
}

/// `now`: past W and G the pool's room as it is now, no wait and no refusal kept (`spill_region_now`).
/// # Safety — As `spill_region`.
pub(super) unsafe fn spill_in(ctx: *mut Ctx, bytes: usize, zeroed: bool, now: bool) -> *mut u8 {
    spill_in_as(ctx, bytes, zeroed, now, false)
}

/// `spill_in`; `spills`: the gate's `Spills` asker past W and G (`spill_region_spills`).
/// # Safety — As `spill_region`.
unsafe fn spill_in_as(ctx: *mut Ctx, bytes: usize, zeroed: bool, now: bool, spills: bool) -> *mut u8 {
    let n = inner_of(ctx);
    // SAFETY: as `Inner::stmt` (null on a worker slot).
    let stmt = unsafe { n.stmt.as_ref() };
    let chunk = match (zeroed, now) {
        _ if spills && !now => crate::node::chunk_region_spills(bytes, Some(&mut n.w), stmt, n.w_only, zeroed),
        (true, false) => crate::node::chunk_region(bytes, Some(&mut n.w), stmt, n.w_only),
        (true, true) => crate::node::chunk_region_now(bytes, Some(&mut n.w), stmt, n.w_only),
        (false, _) => crate::node::chunk_region_unzeroed(bytes, Some(&mut n.w), stmt, n.w_only, now),
    };
    let Some(mut chunk) = chunk else { return core::ptr::null_mut() };
    n.mapped_fresh(chunk.len() as u64);
    // LEDGER2 step 5: the running pipeline's slot owner holds it
    if let Some(o) = n.op_owner {
        chunk.own_by(o);
    }
    let p = chunk.as_mut_ptr();
    n.spills.push(chunk); // alloc: Exec
    p
}

/// D641: moves each time this owner's spill regions are dropped (the statement's end): a holder of their addresses
/// forgets them when it changed.
/// # Safety — `ctx` is bound.
pub unsafe fn spill_epoch(ctx: *mut Ctx) -> u64 {
    inner_of(ctx).spill_epoch
}

/// A zeroed region of `bytes` that outlives the Call: a pool chunk pinned until `Node::reset` (a Call's rewind starts behind `Inner.pinned`); inside a Call the chunks the window is using shift up one index (a rotate, never a swap into the free tail); null when the grant pool refuses (a pin is bounded by the worker's W, else G, never by a chunk count).
/// # Safety — `ctx` is bound.
pub(crate) unsafe fn pin(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
    pin_zeroed(ctx, bytes, bytes)
}

/// `pin` with only the first `zeroed` bytes zeroed (an owner region: section.rs `cut_zeroed`); debug builds and
/// `PGRUST_GRAVITY_SCRIBBLE` runs poison the rest. # Safety — as `pin`.
pub(crate) unsafe fn pin_zeroed(ctx: *mut Ctx, bytes: usize, zeroed: usize) -> *mut u8 {
    pin_road(ctx, bytes, zeroed, false, None)
}

/// C10 (D3582): `pin_zeroed` of an owner-phase breaker's owner region or owners' chunk (section.rs); with `now` its chunk
/// past W and G takes only the pool's room now (`chunk_region_unzeroed`: no wait, no refusal kept), null when it has none.
/// Past W and G it counts as SPILL-DEBT's `AggOwnerAll` road (`grow::uncapped_pin`). # Safety — as `pin`.
pub(crate) unsafe fn pin_owner(ctx: *mut Ctx, bytes: usize, zeroed: usize, now: bool) -> *mut u8 {
    pin_road(ctx, bytes, zeroed, now, Some(gravity_program::spill::NoSpill::AggOwnerAll))
}

/// C10 2b (D3582): `pin_zeroed` of a batched slot's one owner region (mem/batch.rs `pin_region`): planned within W and G,
/// it counts past them as SPILL-DEBT's `AggPartitionPastGrant` road (a partition whose records alone pass the grant, one
/// partition level). # Safety — as `pin`.
pub(crate) unsafe fn pin_batch(ctx: *mut Ctx, bytes: usize, zeroed: usize) -> *mut u8 {
    pin_road(ctx, bytes, zeroed, false, Some(gravity_program::spill::NoSpill::AggPartitionPastGrant))
}

/// D491: `bytes` of a temp file mapped shared for the serial owners the grant pool refused (`section.rs own_split`): held to
/// the statement's end with the pins (`drop_result_spills`, a reset), never charged to the pool, zeroed (a fresh file). Null
/// when the session has no file road (`SpillFactory::map_temp`); an error is the file's own (53400 past temp_file_limit).
/// # Safety — `ctx` is bound.
pub(crate) unsafe fn map_owners(ctx: *mut Ctx, bytes: usize) -> crate::Result<*mut u8> {
    let Some(mut m) = crate::mem::spill::map_temp(bytes)? else { return Ok(core::ptr::null_mut()) };
    let n = inner_of(ctx);
    // LEDGER2 step 5: the running pipeline's slot owner holds it
    if let Some(o) = n.op_owner {
        m.own_by(o);
    }
    let p = m.as_mut_ptr();
    n.owner_maps.push(m); // alloc: Cold
    Ok(p)
}

/// D491: the mapping at `p` (`map_owners`) given back before the statement's end (a split that reruns maps afresh).
/// # Safety — `ctx` is bound; nothing reads the mapping again.
pub(crate) unsafe fn unmap_owners(ctx: *mut Ctx, p: *mut u8) {
    let n = inner_of(ctx);
    if let Some(i) = n.owner_maps.iter().position(|m| m.as_mut_ptr() == p) {
        drop(n.owner_maps.swap_remove(i));
    }
}


/// D3236 (B): the pinned chunk at `p` filed as produced slot `slot`'s one-owner region (`Inner::slot_pins`): `Node::free_slot`
/// gives it back with the slot. Nothing when `slot` is `None` or `p` null (a refused pin). LEDGER2 step 5: the pin of the
/// slot this pipeline produces is charged to the slot's owner (`Inner::op_owner`, the handle the bind set); a pin kept past
/// the slot's free stays on its ended cell to the statement's end, when the chunks settle (`settle_grow_chunks`).
/// # Safety — `ctx` is bound.
pub(crate) unsafe fn file_slot_pin(ctx: *mut Ctx, slot: Option<u8>, p: *mut u8) {
    if let (Some(s), false) = (slot, p.is_null()) {
        let n = inner_of(ctx);
        n.slot_pins.push((s, p as usize)); // alloc: Exec
        if let (Some(o), true) = (n.op_owner, s == n.own_slot) {
            let pinned = n.pinned;
            if let Some(c) = n.chunks[..pinned].iter_mut().find(|c| c.as_ptr() == p.cast_const()) {
                c.own_by(o);
            }
        }
    }
}

/// D4078 (ADMITFP (A)): the owner region at `p` an owner phase pinned for a round 1 its records
/// overflowed at the statement's wall, given back before its split pins a chunk: the pinned chunk leaves the pinned prefix
/// (as `Node::unpin` moves one) and is DROPPED, its holds released and its pages back to the kernel (a large mapping
/// through the reaper), not parked: the split's chunk replaces it at once, and a parked region keeps its pages resident
/// beside it. Only at or over `floor` (`Node::owner_give_floor`, read by the caller with the Node's conditions), never a
/// statement pin; its `slot`'s entry in `slot_pins` (D3236 (B)) leaves with it. The bytes given back (0: kept).
/// # Safety — `ctx` is bound; nothing reads the region again (the split's owners take its place).
pub(crate) unsafe fn give_owner_pin(ctx: *mut Ctx, slot: Option<u8>, p: *mut u8, floor: usize) -> u64 {
    let n = inner_of(ctx);
    let base = p as usize;
    if p.is_null() || n.stmt_pins.contains(&base) {
        return 0;
    }
    let Some(j) = n.chunks[..n.pinned].iter().position(|c| c.as_ptr() as usize == base) else { return 0 };
    if j < floor {
        return 0;
    }
    let c = n.chunks.remove(j);
    (n.pinned, n.chunk_next) = (n.pinned - 1, n.chunk_next - 1);
    if let Some(i) = slot.and_then(|s| n.slot_pins.iter().position(|&e| e == (s, base))) {
        n.slot_pins.swap_remove(i);
    }
    let len = c.len() as u64;
    n.unmapped(len);
    drop(c);
    crate::section::OWN_PIN_GIVEN.fetch_add(len, core::sync::atomic::Ordering::Relaxed);
    len
}

#[inline(always)]
unsafe fn pin_road(ctx: *mut Ctx, bytes: usize, zeroed: usize, now: bool, road: Option<gravity_program::spill::NoSpill>) -> *mut u8 {
    let n = inner_of(ctx);
    let want = bytes.max(CHUNK);
    let idx = match (n.chunk_next..n.chunks.len()).find(|&i| n.chunks[i].len() >= want) {
        Some(i) => i,
        None => {
            // SAFETY: as `Inner::stmt` (null on a worker slot).
            let stmt = unsafe { n.stmt.as_ref() };
            // C10: what W and G held before the draw, for its uncapped check below (a pin past them counts as a grow does)
            let held = (n.w, stmt.map(|s| s.left()));
            // NR-6: a named pin past them the debt lever refuses (`grow::debt_pin_refused`) asks the pool nothing, as a refusal
            let chunk = if crate::mem::grow::debt_pin_refused(&*n, want as u64, held, road) { None } else { crate::node::chunk_region_unzeroed(want, Some(&mut n.w), stmt, n.w_only, now) };
            let Some(chunk) = chunk else {
                if !now {
                    refused(n, want);
                }
                return core::ptr::null_mut();
            };
            crate::mem::grow::uncapped_pin(&*n, want as u64, held, road);
            n.mapped_fresh(chunk.len() as u64);
            n.chunks.push(chunk); // alloc: Exec
            n.chunks.len() - 1
        }
    };
    n.chunks[n.pinned..=idx].rotate_right(1);
    n.pinned += 1;
    n.chunk_next = n.chunk_next.max(n.pinned - 1) + 1;
    let p = n.chunks[n.pinned - 1].as_mut_ptr();
    let zeroed = zeroed.min(bytes);
    if zeroed < bytes && (cfg!(debug_assertions) || crate::sink::scribble()) {
        core::ptr::write_bytes(p.add(zeroed), crate::sink::POISON, bytes - zeroed);
    }
    if zeroed > cancelpoll::PIECE {
        return zero_stepped(p, zeroed);
    }
    core::ptr::write_bytes(p, 0, zeroed);
    p
}

/// D3971 (C9): `len` bytes at `p` zeroed as a step, a piece (`cancelpoll::PIECE`) a tick: a two-round owner's pin zeroes its
/// whole region (a 1.2 GB chunk took 0.6-1.5 s, D3212), which held its thread past a cancel after the pool's wait.
/// Canceled, the pin is refused (null): its caller reads `cancelpoll::seen` as the cancel (D3887 / D3902), and the chunk
/// stays pinned to the node's reset.
/// # Safety — `p` is valid for `len` bytes of writes.
#[cold]
unsafe fn zero_stepped(p: *mut u8, len: usize) -> *mut u8 {
    let mut step = cancelpoll::step("arena pin: a pinned chunk zeroed");
    let zeroed = step.pieces(len, cancelpoll::PIECE, |r| {
        // SAFETY: `r` is inside the caller's `len` bytes.
        unsafe { core::ptr::write_bytes(p.add(r.start), 0, r.len()) };
        Ok::<(), cancelpoll::Canceled>(())
    });
    if zeroed.is_err() {
        return core::ptr::null_mut();
    }
    p
}

/// What a `pin` on the leader can take inside the statement's grant (G's room); `None` = unbounded
/// (the pool unarmed, or no admitted statement on this thread). # Safety — `ctx` is bound.
pub(crate) unsafe fn pin_room(ctx: *mut Ctx) -> Option<u64> {
    let n = inner_of(ctx);
    let s = n.stmt().filter(|s| crate::mem::grant::POOL.capacity() != 0 && (s.holds_slot() || s.small))?;
    Some(n.w + s.left())
}

/// D4078: the one-owner wall the statement's admission sized its owner pins with (`Grant::owner_wall_bytes`, read once at
/// its admission), at most `format`; `format` with no statement (a worker slot). # Safety — `ctx` is bound.
pub(crate) unsafe fn owner_wall_of(ctx: *mut Ctx, format: usize) -> usize {
    inner_of(ctx).stmt().map_or(format, |s| (s.grant.owner_wall_bytes() as usize).min(format) & !7)
}

/// The largest one pin inside the grant (a pin draws the worker's W or the reservation, never both). # Safety — `ctx` is bound.
pub(crate) unsafe fn pin_room_one(ctx: *mut Ctx) -> Option<u64> {
    let n = inner_of(ctx);
    let s = n.stmt().filter(|s| crate::mem::grant::POOL.capacity() != 0 && (s.holds_slot() || s.small))?;
    Some(n.w.max(s.left()))
}

/// NR-3: a kernel pin the pool refused (the refusal kept on the leader's statement, D2845) and the temp-file road too, with
/// no file error held (D4305), in a pipeline whose home names a SPILL-DEBT row (`SlotRegion::debt`): the row, for the
/// kind 2 or the raise the kernel answers the null with (`Inner::settle_call`). A worker keeps no refusal: nothing.
/// # Safety — `ctx` is bound.
#[cold]
unsafe fn pin_unserved(ctx: *mut Ctx) {
    let n = inner_of(ctx);
    // SAFETY: as `Inner::stmt` (null on a worker slot).
    if n.err.is_some() || unsafe { n.stmt.as_ref() }.is_none_or(|s| s.refused() == 0) {
        return;
    }
    if let crate::mem::grow::Home::Slot(r) = n.home {
        n.pin_debt = (*r).debt;
    }
}

gravity_abi::host! {
    /// `Program.pin`: `pin` for a kernel (an owner's images that must outlive the Call: the text keys' arena). # Safety — `ctx` is bound.
    /// `PIN_STATEMENT` (a keyed emit's kept array): the chunk's base is listed in `Inner.stmt_pins`, which `Node::file_pins`
    /// never files as a rerun set's, so it stays pinned to the statement's end.
    pub unsafe fn pin_host(ctx: *mut Ctx, bytes: u64) -> *mut u8 {
        let stmt = bytes & gravity_abi::program::PIN_STATEMENT != 0;
        let bytes = (bytes & !gravity_abi::program::PIN_STATEMENT) as usize;
        guarded((*(*ctx).program.ptr()).env.ptr(), || {
            // D3035: a kernel's pin has no road of its own past the pool (the owners' regions do: own_all / own_split map
            // theirs), so the refused one is carved from a mapped temp file
            let p = pin(ctx, bytes);
            // D3971: a pin refused for the statement's cancel (the pool's wait or the zeroing stopped) maps no temp file
            let p = if p.is_null() && !cancelpoll::seen() { pin_mapped(ctx, bytes) } else { p };
            if p.is_null() && !cancelpoll::seen() {
                pin_unserved(ctx);
            }
            if stmt && !p.is_null() {
                inner_of(ctx).stmt_pins.push(p as usize); // alloc: Exec
            }
            p
        }, |_| core::ptr::null_mut())
    }
}

#[cfg(test)]
mod d3971 {
    /// D3971: a pinned chunk's zeroing past one `cancelpoll::PIECE` stops before its next piece once its statement is
    /// canceled (null: the pin refused, `seen` the cancel); outside a kernel call it zeroes the whole length.
    #[test]
    fn a_canceled_pin_stops_zeroing_at_its_next_piece() {
        let len = 3 * cancelpoll::PIECE + 8;
        let mut v = vec![0xA5u8; len]; // alloc: Cold
        let p = v.as_mut_ptr();
        let byte = core::sync::atomic::AtomicU8::new(cancelpoll::ABORT_BIT);
        let got = cancelpoll::scoped(&byte, || {
            // SAFETY: `p` holds `len` bytes.
            let got = unsafe { super::zero_stepped(p, len) };
            assert!(cancelpoll::seen(), "the stop is the call's cancel");
            got
        });
        assert!(got.is_null(), "a canceled zeroing refuses the pin");
        assert!(v.iter().all(|&b| b == 0xA5), "it stopped before its first piece");
        // SAFETY: as above.
        assert_eq!(unsafe { super::zero_stepped(p, len) }, p);
        assert!(v.iter().all(|&b| b == 0), "outside a kernel call it zeroes the whole length");
    }
}
