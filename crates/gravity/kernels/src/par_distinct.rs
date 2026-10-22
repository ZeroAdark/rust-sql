//! `SELECT k, count(DISTINCT x) FROM t GROUP BY k` (§2 item 5, partitioned-record.md §3.5, ruling
//! 5): phase 1 is the identity set of `(k, x)` pairs in the share's phase-1 table (no slots: a pair
//! folds into itself; the home pair probed, the home's occupant evicted, the append bypass by
//! observation, the table drained at the claim's end), partitioned by the pair's hash so one group's
//! pairs spread over every partition. Round 1: an owner folds a partition's pairs (the fold of a
//! pair past the first is a no-op) and counts them per `k` into its round-2 phase-1 table, keyed
//! by `k` — its evictions and its drain at the round's end are the re-sink by the group hash.
//! Round 2: an owner folds a partition's `(k) → count` records across the owners (the counts add)
//! and writes them as one run sorted by `k`; the emit merges every run in key order (the width-1
//! arm's one owner folds on the backend thread, `Entry::own_single`).

use gravity_abi::PtrWord;
use gravity_abi::ctx::{hdr, resume, RESUME_NONE};
use gravity_abi::status::{suspend, unsupported, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
use gravity_abi::{Ctx, Lane, Site};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::breaker::{table_kind, Breaker, BreakerKind, Budget, HashAgg, KeyKind, Owned, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, Spill, T1, ADAPT_FACTOR, HASHAGG_HDR, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS, REDUCE_FLUSH, SHARE, T1_BYPASS, t1_default_bytes, T1_MAX_PARTS, TABLE_OWNED};
use gravity_abi::hash::KeyHash;
use gravity_abi::program::SPILL_SCATTER;
use crate::util::{HOST_CONTINUE, HOST_STOP};

use crate::ops::sort::runmerge::MERGING;
use crate::util::{advance, emit, end, flush, for_each_row, int8_datum, lane, partial, range_seek, site as site_of, Cell};
use crate::{Touch, SITE_EXEC_CELLS};
use crate::table::image::AlignedImage;

/// The pair record: `(k, x)` and the implicit count sentinel (a pair's multiplicity, unused).
pub const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I32, KeyKind::I32], &[]);
/// The group record: `(k) → pairs`.
pub const LAYOUT2: RecordLayout = RecordLayout::new_nn(&[KeyKind::I32], &[SlotKind::Count]);
pub const INIT: u32 = 256;
pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };
pub const PARAM_SPILL_BITS: usize = 0;
pub const NPARAMS: usize = 1;
pub const DEFAULT_BITS: u8 = 4;
/// An owner's run directory: one run per round-2 partition it folded.
pub const MAX_RUNS: usize = T1_MAX_PARTS as usize;
/// The round-2 table of the owner (the hot groups; the pool is the rest of its share).
const ROUND2_T1_BYTES: u32 = 256 << 10;

const REC: usize = LAYOUT.size() as usize;
const REC2: usize = LAYOUT2.size() as usize;

pub const fn spill_bits(pairs: u64) -> u8 {
    crate::table::params::spill_bits(pairs, LAYOUT.size())
}

pub const fn params(pairs: u64) -> [u64; NPARAMS] {
    [spill_bits(pairs) as u64]
}

pub const fn breaker(pairs: u64) -> Breaker {
    Breaker { kind: BreakerKind::Distinct, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: spill_bits(pairs) } }
}

fn bits_of(p: &[u64]) -> u8 {
    let b = p[PARAM_SPILL_BITS] as u8;
    if b == 0 { DEFAULT_BITS } else { b }
}

/// # Safety
/// `ctx` is laid out per `programs::par_distinct`: `partial` at a share-armed region, `NPARAMS` words.
pub unsafe extern "C-unwind" fn par_distinct(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let bits = bits_of(core::slice::from_raw_parts((*ctx).params.ptr(), NPARAMS));
    // the partition count: the plan's, floored at 64 for a section of width 8 or more (ruling 12)
    let pbits = gravity_abi::breaker::opened_bits(bits, (*ctx).width, false);
    // a raw pointer: the spill host rewrites the pool mid-fold
    let table = T1::at(partial(ctx), &LAYOUT, pbits, t1_default_bytes(), SHARE) as *mut T1;
    let est = (1u64 << bits) * (512 << 10) / REC as u64;
    if est > ADAPT_FACTOR * (*table).cap as u64 {
        (*table).flags |= T1_BYPASS;
    }
    let bypass = (*table).flags & T1_BYPASS != 0;
    let mut st = Sink { ctx, table, pbits, bypass, last: [0; 2], last_rec: core::ptr::null_mut() };
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            if r == 0 {
                // the claim's end: the table drained into the buffers, so phase 2 reads buffers only
                while !T1::drain(table) {
                    if let Err(e) = st.flush() {
                        return e;
                    }
                }
            }
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, 0);
        let (lk, lx) = (lane(s, 0), lane(s, 1));
        let code = crate::by_width!(lk.width, pairs_k(s, lk, lx, &mut st));
        if code < 0 {
            return code;
        }
        s.cursor = end(s);
    }
}

/// The phase-1 fold's state: the share's `T1`, its partition count, the append bypass with its run collapse.
struct Sink {
    ctx: *mut Ctx,
    table: *mut T1,
    pbits: u8,
    bypass: bool,
    last: [u64; 2],
    last_rec: *mut u8,
}

impl Sink {
    unsafe fn flush(&mut self) -> Result<(), i64> {
        if crate::util::spill(self.ctx, SPILL_SCATTER, self.pbits as i64) < 0 {
            return Err(ERROR);
        }
        self.last_rec = core::ptr::null_mut();
        Ok(())
    }

    unsafe fn switch_to_append(&mut self) -> Result<(), i64> {
        while !T1::drain(self.table) {
            self.flush()?;
        }
        (*self.table).flags |= T1_BYPASS;
        self.bypass = true;
        Ok(())
    }

    /// The pair's record: found or opened at its home pair (the home's occupant evicted into its
    /// buffer on a miss), or appended behind a run collapse (the bypass); a full pool flushes first.
    #[inline(always)]
    unsafe fn record(&mut self, keys: &[u64; 2]) -> Result<(), i64> {
        if self.bypass {
            if !self.last_rec.is_null() && self.last == *keys {
                return Ok(());
            }
            let part = KeyHash::of(keys).part(self.pbits as u32);
            let r = match T1::append_new(self.table, part, keys) {
                Some(r) => r,
                None => {
                    self.flush()?;
                    T1::append_new(self.table, part, keys).ok_or(unsupported(UNSUPPORTED_SCRATCH))?
                }
            };
            self.last = *keys;
            self.last_rec = r;
            return Ok(());
        }
        let hash = KeyHash::of(keys);
        if T1::probe(self.table, keys, hash).is_some() {
            return Ok(());
        }
        let home = (*self.table).home(hash);
        if !T1::evict(self.table, home) {
            self.flush()?;
            T1::probe(self.table, keys, hash).ok_or(unsupported(UNSUPPORTED_SCRATCH))?;
            return Ok(());
        }
        T1::open_at(self.table, home, keys);
        if (*self.table).observe() {
            // the record just opened is drained with the rest (a pair is its own identity: nothing to carry)
            self.switch_to_append()?;
        }
        Ok(())
    }
}

#[inline(always)]
unsafe fn pairs_k<K: Cell>(s: &mut Site, lk: &Lane, lx: &Lane, st: &mut Sink) -> i64 {
    crate::by_width!(lx.width, pairs::<K>(s, lk, lx, st))
}

/// Every `(k, x)` pair with both non-null into the set.
#[inline(always)]
unsafe fn pairs<K: Cell, X: Cell>(s: &mut Site, lk: &Lane, lx: &Lane, st: &mut Sink) -> i64 {
    let (kp, xp) = (lk.ptr.ptr() as *const K, lx.ptr.ptr() as *const X);
    let mut out = 0u64;
    let mut code = HOST_CONTINUE;
    let ok = for_each_row(s, |i| {
        if !lk.is_valid(i) || !lx.is_valid(i) {
            return true;
        }
        out += 1;
        let key = AlignedImage([(*kp.add(i)).i32() as i64 as u64, (*xp.add(i)).i32() as i64 as u64]);
        if let Err(e) = st.record(&key.0) {
            code = e;
            return false;
        }
        true
    });
    s.rows_out += out;
    if ok {
        HOST_CONTINUE
    } else {
        code
    }
}

// ---- the owner phase (§3.5): two rounds ----

/// The owner functions (the spill bits by value): two rounds, no count order.
pub fn owner_fns(params: &[u64]) -> OwnerFns {
    let mut p = [0u64; OWNER_PARAMS];
    p[..NPARAMS].copy_from_slice(&params[..NPARAMS]);
    OwnerFns { params: p, rounds: 2, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: owner_min, open: owner_open, reduce: owner_reduce, end_round: owner_end, reduce2: owner_reduce2, fold: Some(crate::table::owner_fold::owner_fold) }
}

/// The owner region: `OwnerTop` (`worst` = the runs, `total` = the run bytes, `len` = the groups),
/// the run directory `(offset, records)` per run, the run area, the round-2 phase-1 table with its
/// pool (`round2_off`), the round-1 table space (`table_off`): a quarter, a quarter and a half of the rest.
const DIR_OFF: usize = OWNERTOP_HDR;
const RUN_OFF: usize = DIR_OFF + 8 * MAX_RUNS;

fn owner_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `NPARAMS` plan words.
    let bits = bits_of(unsafe { core::slice::from_raw_parts(params, NPARAMS) });
    // the groups are bounded by the pairs; the runs and the round-2 pool each hold this owner's share
    let est = (1u64 << bits) * (512 << 10) / REC as u64;
    RUN_OFF as u64 + 2 * (est * REC2 as u64 / 2).max(ROUND2_T1_BYTES as u64 + 2048 * 12 * 2) + (HASHAGG_HDR + 64 * REC) as u64
}

unsafe fn owner_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let bits = bits_of(core::slice::from_raw_parts(params, NPARAMS));
    let rest = bytes.saturating_sub(RUN_OFF);
    let quarter = (rest / 4) & !7;
    let round2_off = RUN_OFF + quarter;
    let table_off = round2_off + quarter;
    if quarter < ROUND2_T1_BYTES as usize / 4 || table_off + HASHAGG_HDR + 2 * REC > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let r2 = owner.add(round2_off);
    core::ptr::write_bytes(r2, 0, 64);
    T1::at(r2, &LAYOUT2, bits.max(3), (quarter as u32 / 2).min(ROUND2_T1_BYTES), quarter as u32);
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: 0, table_off: table_off as u32, worst: 0, round2_off: round2_off as u32, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// Round 1: the partition's deduped pairs counted per group into the owner's round-2 table.
unsafe fn owner_reduce(_params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let h = &mut *(owner as *mut OwnerTop);
    let t = &*table;
    if h.resume == 0 {
        h.records += t.len as u64;
    }
    let r2 = owner.add(h.round2_off as usize) as *mut T1;
    for i in h.resume as usize..t.cap as usize {
        let r = t.record(i);
        if !r.used() {
            continue;
        }
        let keys = [r.key(0)];
        let hash = KeyHash::of(&keys);
        let p = match T1::probe(r2, &keys, hash) {
            Some(p) => p,
            None => {
                let home = (*r2).home(hash);
                if !T1::evict(r2, home) {
                    // a full pool: the driver flushes the buffers to the owner's round-2 file and calls again
                    h.resume = i as u32;
                    return REDUCE_FLUSH;
                }
                T1::open_at(r2, home, &keys)
            }
        };
        Rec::at(p, &LAYOUT2).add_count(0, 1);
    }
    h.resume = 0;
    0
}

/// After the owner's last round-1 partition: its round-2 table drained into the round-2 buffers.
unsafe fn owner_end(_params: *const u64, owner: *mut u8) -> i64 {
    let h = &*(owner as *const OwnerTop);
    if !T1::drain(owner.add(h.round2_off as usize) as *mut T1) {
        return REDUCE_FLUSH;
    }
    0
}

/// Round 2: a partition's group records folded across the owners, written as one run sorted by `k`.
unsafe fn owner_reduce2(_params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    let n = t.len as usize;
    h.groups += n as u64;
    if n == 0 {
        return 0;
    }
    t.sort_dense_by_key(0);
    let runs = h.worst as usize;
    let off = RUN_OFF + h.total as usize;
    if runs >= MAX_RUNS || off + n * REC2 > h.round2_off as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    core::ptr::copy_nonoverlapping(t.base_ptr(), owner.add(off), n * REC2);
    let dir = owner.add(DIR_OFF) as *mut u32;
    *dir.add(2 * runs) = off as u32;
    *dir.add(2 * runs + 1) = n as u32;
    h.worst = runs as u32 + 1;
    h.total += (n * REC2) as u64;
    h.len += n as u32;
    0
}

// ---- the emit ----

unsafe fn emit_pair(ctx: *mut Ctx, k: u64, n: i64) -> i64 {
    let v = [k, int8_datum(n)];
    let nl = [0u8; 2];
    let f = emit(ctx, 2, &[(v.as_ptr(), nl.as_ptr(), 2)]);
    if f < 0 {
        return ERROR;
    }
    f
}

/// # Safety
/// `ctx` is laid out per `programs::par_distinct` with `partial` at the owners' results (`TABLE_OWNED`).
pub unsafe extern "C-unwind" fn par_distinct_emit(ctx: *mut Ctx) -> i64 {
    let own = partial(ctx);
    if table_kind(own) != TABLE_OWNED {
        return unsupported(UNSUPPORTED_SHAPE);
    }
    emit_owned(ctx, own, (*ctx).resume)
}

/// The owners' runs (each sorted by `k`, a key in one run only) merged in key order: the heap and
/// the positions in owner 0's table space, so a stop resumes the merge.
unsafe fn emit_owned(ctx: *mut Ctx, own: *mut u8, word: i64) -> i64 {
    let o = Owned::view(own);
    let w = o.nowners as usize;
    if w > 64 {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let mut nruns = 0usize;
    for j in 0..w {
        nruns += OwnerTop::view(o.owner(j)).worst as usize;
    }
    let run = |g: usize| -> (*mut u8, usize) {
        let mut g = g;
        for j in 0..w {
            let h = OwnerTop::view(o.owner(j));
            if g < h.worst as usize {
                let dir = o.owner(j).add(DIR_OFF) as *const u32;
                return (o.owner(j).add(*dir.add(2 * g) as usize), *dir.add(2 * g + 1) as usize);
            }
            g -= h.worst as usize;
        }
        (core::ptr::null_mut(), 0)
    };
    let h0 = OwnerTop::view(o.owner(0));
    let state = o.owner(0).add(h0.table_off as usize) as *mut u32;
    if h0.table_off as usize + 4 + 8 * nruns > o.owner_bytes as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let heap = state.add(1);
    let pos = heap.add(nruns);
    let head = move |g: usize| -> i64 {
        let (base, _) = run(g);
        *(base.add(*pos.add(g) as usize * REC2) as *const i64)
    };
    let before = move |a: usize, b: usize| -> bool {
        let (ka, kb) = (head(a), head(b));
        ka < kb || (ka == kb && a < b)
    };
    let sift_down = move |n: usize, mut i: usize| loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < n && before(*heap.add(l) as usize, *heap.add(m) as usize) {
            m = l;
        }
        if r < n && before(*heap.add(r) as usize, *heap.add(m) as usize) {
            m = r;
        }
        if m == i {
            break;
        }
        core::ptr::swap(heap.add(i), heap.add(m));
        i = m;
    };
    if word == RESUME_NONE {
        let mut n = 0usize;
        for g in 0..nruns {
            *pos.add(g) = 0;
            if run(g).1 > 0 {
                *heap.add(n) = g as u32;
                n += 1;
            }
        }
        *state = n as u32;
        for i in (0..n / 2).rev() {
            sift_down(n, i);
        }
    }
    while *state > 0 {
        let g = *heap as usize;
        let (base, len) = run(g);
        let r = Rec::at(base.add(*pos.add(g) as usize * REC2), &LAYOUT2);
        *pos.add(g) += 1;
        if *pos.add(g) as usize >= len {
            *state -= 1;
            *heap = *heap.add(*state as usize);
        }
        sift_down(*state as usize, 0);
        let f = emit_pair(ctx, r.key(0), r.count(0));
        if f < 0 {
            return ERROR;
        }
        if f == HOST_STOP {
            (*ctx).resume = resume::word(resume::EMIT, MERGING);
            return suspend(crate::util::out_count(ctx) == 0);
        }
    }
    (*ctx).resume = RESUME_NONE;
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}
