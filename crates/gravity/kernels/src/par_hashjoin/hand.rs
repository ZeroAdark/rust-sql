//! The `par_hashjoin_build` / `par_hashjoin_probe` kernels and the spilled part (the params, the spill helpers and the outer split `hashjoin_part` drives stay in `par_hashjoin.rs`).
#![allow(unused_imports)]
use gravity_abi::PtrWord;
use super::*;
use crate::util::{advance, consumed, done, emit, end, flush, index, lane, partial, range_seek, site as site_of, Cell, RESUME_OPEN};
use crate::util::{HOST_CONTINUE, HOST_STOP};
use gravity_abi::program::SPILL_SCATTER;
use gravity_abi::breaker::{HashBuild, BUILD_PARTITION, BUILD_TABLE, USED_NULL};
use gravity_abi::status::DONE;
use gravity_abi::{Lane, Site};
use gravity_abi::status::{suspend, unsupported, UNSUPPORTED_SCRATCH};
use gravity_abi::ctx::RESUME_NONE;

unsafe fn bits(ctx: *mut Ctx) -> i64 {
    let b = *(*ctx).params.ptr().add(PARAM_SPILL_BITS) as u8;
    if b == 0 {
        DEFAULT_BITS as i64
    } else {
        b as i64
    }
}

/// `SPILL_PARTS` / `SPILL_REFILL` arguments name a stream by `slot + 1`.
const fn stream(slot: u8) -> i64 {
    slot as i64 + 1
}

/// # Safety
/// `ctx` is laid out per `programs::par_hashjoin`: `partial` at a share-armed region (the
/// worker's build partition), `NPARAMS` parameter words.
pub unsafe extern "C-unwind" fn par_hashjoin_build(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, INNER) < 0 {
        return ERROR;
    }
    // the table as a raw pointer: the spill host clears it mid-loop, which a `&mut` (noalias) would let the optimizer ignore
    let build = HashBuild::at_share(partial(ctx), BUILD_PARTITION) as *mut HashBuild;
    let bits = bits(ctx);
    loop {
        let r = advance(ctx, INNER);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, INNER);
        let (lk, la) = (lane(s, 0), lane(s, 1));
        let ok = crate::by_width!(lk.width, insert_k(ctx, s, lk, la, build, bits));
        if ok < 0 {
            return ok;
        }
        s.cursor = end(s);
    }
}

#[inline(always)]
unsafe fn insert_k<K: Cell>(ctx: *mut Ctx, s: &mut Site, lk: &Lane, la: &Lane, build: *mut HashBuild, bits: i64) -> i64 {
    crate::by_width!(la.width, insert::<K>(ctx, s, lk, la, build, bits))
}

/// Append every valid-key row of the window; a full partition scatters into an epoch and goes on.
#[inline(always)]
unsafe fn insert<K: Cell, A: Cell>(ctx: *mut Ctx, s: &mut Site, lk: &Lane, la: &Lane, build: *mut HashBuild, bits: i64) -> i64 {
    let (kp, ap) = (lk.ptr.ptr() as *const K, la.ptr.ptr() as *const A);
    let mut out = 0u64;
    let mut code = 0i64;
    let ok = crate::util::for_each_row(s, |i| {
        if !lk.is_valid(i) {
            return true;
        }
        let (payload, used) = if la.is_valid(i) { ((*ap.add(i)).i64(), 1) } else { (0, USED_NULL) };
        out += 1;
        let k = (*kp.add(i)).i32();
        if !(*build).insert_tagged(k, payload, used) {
            if spill(ctx, SPILL_SCATTER, bits) < 0 {
                code = ERROR;
                return false;
            }
            if !(*build).insert_tagged(k, payload, used) {
                code = unsupported(UNSUPPORTED_SCRATCH);
                return false;
            }
        }
        true
    });
    s.rows_out += out;
    if ok {
        0
    } else {
        code
    }
}

/// The probe over the outer (§2 decision 9's `cont`): every outer row probes the merged table,
/// a stop mid-chain saving the chain position in `Site.cont`; with the build spilled the outer rows are kept instead (`keep_outer`) for the third phase.
/// # Safety
/// `ctx` is laid out per `programs::par_hashjoin`: `consumed[0]` the merged build, `partial` the share-armed outer buffer.
pub unsafe extern "C-unwind" fn par_hashjoin_probe(ctx: *mut Ctx) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        if range_seek(ctx, OUTER) < 0 {
            return ERROR;
        }
        site_of(ctx, OUTER).cont = 0;
        (*ctx).resume = RESUME_OPEN;
    }
    let spilled = spill(ctx, SPILL_PARTS, stream(SLOT_BUILD));
    if spilled < 0 {
        return ERROR;
    }
    if spilled > 0 {
        return keep_outer(ctx);
    }
    let table = HashBuild::view(consumed(ctx, 0));
    loop {
        let r = advance(ctx, OUTER);
        if r <= 0 {
            return if r == 0 { done(ctx) } else { r };
        }
        let s = site_of(ctx, OUTER);
        let (lk, lb) = (lane(s, 0), lane(s, 1));
        let f = crate::by_width!(lk.width, probe_k(ctx, s, lk, lb, table));
        if f < 0 {
            return ERROR;
        }
        if f == HOST_STOP {
            // the stop honoured where it fell: `cursor` is the row and `cont` its chain position
            let empty = crate::util::out_count(ctx) == 0;
            if flush(ctx) < 0 {
                return ERROR;
            }
            return suspend(empty);
        }
        s.cursor = end(s);
    }
}

/// The build spilled: the outer rows go to the own buffer, scattered by the join's bits (the site
/// derived here, never a `&mut Site` parameter: the window host writes it, `breakers.md`).
unsafe fn keep_outer(ctx: *mut Ctx) -> i64 {
    let buf = HashBuild::at_share(partial(ctx), BUILD_PARTITION) as *mut HashBuild;
    let bits = bits(ctx);
    loop {
        let r = advance(ctx, OUTER);
        if r < 0 {
            return r;
        }
        if r == 0 {
            // the claim's end: the remainder joins the files, so the third phase reads nothing from regions
            if (*buf).len > 0 && spill(ctx, SPILL_SCATTER, bits) < 0 {
                return ERROR;
            }
            return done(ctx);
        }
        let s = site_of(ctx, OUTER);
        let (lk, lb) = (lane(s, 0), lane(s, 1));
        let ok = crate::by_width!(lk.width, insert_k(ctx, s, lk, lb, buf, bits));
        if ok < 0 {
            return ok;
        }
        s.cursor = end(s);
    }
}

#[inline(always)]
unsafe fn probe_k<K: Cell>(ctx: *mut Ctx, s: &mut Site, lk: &Lane, lb: &Lane, table: &HashBuild) -> i64 {
    crate::by_width!(lb.width, probe_rows::<K>(ctx, s, lk, lb, table))
}

/// Emit one build match against outer key `k` / payload `(b, bn)`.
#[inline(always)]
unsafe fn emit_match(ctx: *mut Ctx, k: i32, e: &gravity_abi::breaker::HashBuildEntry, b: u64, bn: u8) -> i64 {
    let v = [k as i64 as u64, e.payload as u64, b];
    let n = [0u8, (e.used == USED_NULL) as u8, bn];
    emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)])
}

/// Row `j + PROBE_AHEAD`'s home line is prefetched while row `j` probes: the misses overlap instead of retiring one per row (`breakers.md` "Pass 4").
const PROBE_AHEAD: u32 = 16;

#[inline(always)]
unsafe fn prefetch(p: *const u8) {
    #[cfg(target_arch = "aarch64")]
    core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    #[cfg(target_arch = "x86_64")]
    core::arch::x86_64::_mm_prefetch(p as *const i8, core::arch::x86_64::_MM_HINT_T0);
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = p;
}

/// `key`'s home slot (`probe`'s start in its `partition_of` slice); every index is masked, so an unvalidated key still lands inside the table.
#[inline(always)]
unsafe fn prefetch_home(table: &HashBuild, key: i32) {
    let sub = (table.cap as usize) >> table.pbits;
    let slot = HashBuild::partition_of(key, table.pbits) as usize * sub + table.probe(key).position();
    prefetch(table.entry(slot) as *const gravity_abi::breaker::HashBuildEntry as *const u8);
}

#[inline(always)]
unsafe fn probe_rows<K: Cell, B: Cell>(ctx: *mut Ctx, s: &mut Site, lk: &Lane, lb: &Lane, table: &HashBuild) -> i64 {
    let (kp, bp) = (lk.ptr.ptr() as *const K, lb.ptr.ptr() as *const B);
    let mut out = 0u64;
    let mut j = s.cursor;
    let to = end(s);
    while j < to {
        if j + PROBE_AHEAD < to {
            prefetch_home(table, (*kp.add(index(s, j + PROBE_AHEAD))).i32());
        }
        let i = index(s, j);
        if !lk.is_valid(i) {
            s.cont = 0;
            j += 1;
            continue;
        }
        let k = (*kp.add(i)).i32();
        let (b, bn) = ((*bp.add(i)).i64() as u64, !lb.is_valid(i) as u8);
        // a chain stopped mid-way resumes at its saved position; a fresh row counts once
        let mut p = if s.cont != 0 { table.probe_from(k, s.cont as usize - 1) } else { out += 1; table.probe(k) };
        while let Some(e) = p.next() {
            let f = emit_match(ctx, k, e, b, bn);
            if f < 0 {
                s.rows_out += out;
                return f;
            }
            if f == HOST_STOP {
                s.cursor = j;
                s.cont = p.position() as u64 + 1;
                s.rows_out += out;
                return HOST_STOP;
            }
        }
        s.cont = 0;
        j += 1;
    }
    s.cursor = j;
    s.rows_out += out;
    HOST_CONTINUE
}

/// The third phase, one build partition per claim unit (`breakers.md`): the build partition
/// refilled into the scratch's table, the outer partition into its buffer, every buffered row
/// probing the table; a split on either side is followed through the queue; a stop from the
/// sink is not a suspend point (a claim is never re-entered).
/// # Safety
/// `ctx` is laid out per `programs::par_hashjoin`: `partial` a share-armed scratch region,
/// `hdr.claim` a range of build-stream tokens.
pub unsafe extern "C-unwind" fn par_hashjoin_part(ctx: *mut Ctx) -> i64 {
    let own = partial(ctx);
    let (build, outer, outer_off) = scratch_tables(own);
    let outer_parts = spill(ctx, SPILL_PARTS, stream(SLOT_OUTER));
    if outer_parts < 0 {
        return ERROR;
    }
    let claim = (*ctx).claim;
    if outer_parts == 0 {
        return done(ctx);
    }
    // build tokens to join: a split pushes its eight sub-tokens behind the claim's own; an outer
    // partition split once is remembered, so the build sub-tokens reuse its sub-partitions
    let mut stack = [(0u32, 0u32); STACK];
    let mut sp = 0usize;
    let mut memo = [(u32::MAX, 0u32, 0u32); MEMO];
    for t in claim.lo..claim.hi {
        stack[sp] = (t as u32, t as u32);
        sp += 1;
        while sp > 0 {
            sp -= 1;
            let (token, outer_token) = stack[sp];
            let before = spill(ctx, SPILL_PARTS, stream(SLOT_BUILD));
            if before < 0 || spill(ctx, SPILL_TARGET, 0) < 0 {
                return ERROR;
            }
            let n = spill(ctx, SPILL_REFILL, token as i64 | (stream(SLOT_BUILD) << 32));
            if n < 0 {
                return ERROR;
            }
            let after = spill(ctx, SPILL_PARTS, stream(SLOT_BUILD));
            if after < 0 {
                return ERROR;
            }
            if n == 0 && after > before {
                // the build partition split by its next hash window: the outer partition splits by
                // the same window (remembered, or forced on one that fit), and build sub-token
                // `before + i` joins outer sub-token `olo + i` alone — the outer partition is read
                // once per split, not once per build sub-token (8× per generation before)
                let olo = match outer_split(ctx, outer_off, outer_token, &mut memo, stream(SLOT_OUTER)) {
                    Ok((olo, ohi)) => {
                        if ohi - olo != (after - before) as u32 {
                            return unsupported(UNSUPPORTED_SCRATCH);
                        }
                        olo
                    }
                    Err(e) => return e,
                };
                if sp + (after - before) as usize > STACK {
                    return unsupported(UNSUPPORTED_SCRATCH);
                }
                for i in (0..(after - before) as u32).rev() {
                    stack[sp] = (before as u32 + i, olo + i);
                    sp += 1;
                }
                continue;
            }
            if (*build).len == 0 {
                continue;
            }
            let r = join_outer(ctx, build, outer, outer_off, outer_token, &mut memo);
            if r < 0 {
                return r;
            }
        }
    }
    done(ctx)
}

/// The scratch region's two tables: laid out at the first touch from the armed share (the build
/// table over the first half, the outer buffer over the rest), read back afterwards.
unsafe fn scratch_tables(own: *mut u8) -> (*mut HashBuild, *mut HashBuild, i64) {
    let armed = HashBuild::armed(own);
    if armed != 0 {
        (*(own as *mut HashBuild)).table = 0;
        let half = (armed / 2) & !7;
        // the build table opens small and grows inside its half; the outer buffer takes the rest
        let build = HashBuild::at_sized(own, half, BUILD_TABLE, 1024) as *mut HashBuild;
        core::ptr::write_bytes(own.add(half as usize), 0, gravity_abi::breaker::HASHBUILD_HDR);
        let cap_o = HashBuild::cap_of(armed - half);
        if cap_o == 0 {
            gravity_abi::limits::exceeded(format_args!("a share too small for the join's scratch: {armed} bytes"));
        }
        let outer = HashBuild::at(own.add(half as usize), cap_o, BUILD_PARTITION) as *mut HashBuild;
        return (build, outer, half as i64);
    }
    let half = HashBuild::view(own).region;
    let cap_o = HashBuild::view(own.add(half as usize)).cap;
    (own as *mut HashBuild, HashBuild::at(own.add(half as usize), cap_o, BUILD_PARTITION) as *mut HashBuild, half as i64)
}

unsafe fn join_outer(ctx: *mut Ctx, build: *mut HashBuild, outer: *mut HashBuild, outer_off: i64, token: u32, memo: &mut [(u32, u32, u32); MEMO]) -> i64 {
    let mut stack = [0u32; STACK];
    let mut sp = 1usize;
    stack[0] = token;
    while sp > 0 {
        sp -= 1;
        let t = stack[sp];
        if let Some(&(_, lo, hi)) = memo.iter().find(|m| m.0 == t) {
            if sp + (hi - lo) as usize > STACK {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
            for t2 in (lo..hi).rev() {
                stack[sp] = t2;
                sp += 1;
            }
            continue;
        }
        let before = spill(ctx, SPILL_PARTS, stream(SLOT_OUTER));
        if before < 0 || spill(ctx, SPILL_TARGET, outer_off) < 0 {
            return ERROR;
        }
        let n = spill(ctx, SPILL_REFILL, t as i64 | (stream(SLOT_OUTER) << 32));
        if n < 0 {
            return ERROR;
        }
        let after = spill(ctx, SPILL_PARTS, stream(SLOT_OUTER));
        if after < 0 {
            return ERROR;
        }
        if n == 0 && after > before {
            if sp + (after - before) as usize > STACK {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
            if let Some(slot) = memo.iter_mut().find(|m| m.0 == u32::MAX) {
                *slot = (t, before as u32, after as u32);
            }
            for t2 in (before as u32..after as u32).rev() {
                stack[sp] = t2;
                sp += 1;
            }
            continue;
        }
        for i in 0..(*outer).len as usize {
            let e = *(*outer).entry(i);
            let (b, bn) = (e.payload as u64, (e.used == USED_NULL) as u8);
            for m in (*build).probe(e.key) {
                let f = emit_match(ctx, e.key, m, b, bn);
                if f < 0 {
                    return f;
                }
            }
        }
    }
    HOST_CONTINUE
}
