//! The `par_hashagg` kernel: the accumulator front, the fold, the owner set and the emits (the params, `spill_bits` and the layout stay in `par_hashagg.rs`).
//! partitioned-record.md §3.2, §3.4, §3.6): phase 1 folds each row into the share's phase-1 table
//! (`T1`: the home pair probed, the home's occupant evicted into its partition buffer, the
//! append bypass by observation, the table drained at the claim's end) — a dense key domain whose
//! cells fit the cache (`DenseFront::acc_fits`) accumulates in plain cells behind the table and folds into records at the
//! claim's end; the zone top-k keeps its table for the walk's bound and filters rows past it.
//! Each owner folds its partitions and keeps either the `n` smallest keys (the zone top-k, an
//! index heap) or every group as one run per partition — sorted by key under `PARAM_ORDER` —
//! in its region; the emit merges the owners' results (the width-1 arm's one owner folds on the
//! backend thread, `Entry::own_single`).
#![allow(unused_imports)]
use gravity_abi::PtrWord;
use crate::table::params::*;
pub(crate) use crate::util::{advance, emit, end, flush, int8_datum, lane, partial, range_seek, site as site_of, Cell};
pub(crate) use crate::ops::sort::runmerge::MERGING;
pub(crate) use crate::util::HOST_STOP;
pub(crate) use gravity_abi::program::SPILL_SCATTER;
pub(crate) use gravity_abi::breaker::{table_kind, AccCell, HashAgg, Owned, OwnerFns, OwnerTop, Rec, T1, ACC_CELL, ADAPT_FACTOR, HASHAGG_HDR, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS, SHARE, T1_BYPASS, t1_default_bytes, TABLE_OWNED, MAX_OWNERS};
pub(crate) use gravity_abi::hash::KeyHash;
pub(crate) use gravity_abi::status::{DONE, ERROR};
pub(crate) use gravity_abi::{Ctx, Lane, Site};
pub(crate) use gravity_abi::status::{suspend, unsupported, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
pub(crate) use gravity_abi::ctx::{resume, RESUME_NONE};

pub(crate) struct Params {
    n: u32,
    lo: i64,
    hi: i64,
    bits: u8,
    ordered: bool,
}

pub(crate) fn params_of(p: &[u64]) -> Params {
    let bits = p[PARAM_SPILL_BITS] as u8;
    Params { n: (p[PARAM_TOPK_N] as u32).min(MAX_N), lo: p[PARAM_DENSE_LO] as i64, hi: p[PARAM_DENSE_HI] as i64, bits: if bits == 0 { DEFAULT_BITS } else { bits }, ordered: p[PARAM_ORDER] != 0 }
}

pub(crate) unsafe fn params(ctx: *mut Ctx) -> Params {
    params_of(core::slice::from_raw_parts((*ctx).params.ptr(), NPARAMS))
}

pub(crate) const OFF_SUM: usize = LAYOUT.slot_off[0] as usize;

pub(crate) const OFF_CNT: usize = LAYOUT.slot_off[1] as usize;

/// The validity word's low byte: bit 0 the sum's, bit 1 the count's.
pub(crate) const OFF_VALID: usize = LAYOUT.valid_off as usize;

pub(crate) const REC: usize = LAYOUT.size() as usize;

/// `T1.flags` bit 1 (this kernel's): the accumulator cells lie behind the table's limit.
pub(crate) const T1_ACC: u8 = 2;

/// The accumulator cells (decision 7, pass 3 item 1) behind the phase-1 table when the domain is
/// one the plan let through (`DenseFront::acc_fits`) and the armed share holds both: `(cells, dn, the table's limit)` — laid out
/// at the region's first touch (the armed `limit` is the share), found again by `T1_ACC`, which
/// the caller sets once the table is laid out (`T1::at`'s first layout writes the whole header).
pub(crate) unsafe fn acc_front(own: *mut u8, lo: i64, hi: i64, topk: bool) -> (*mut AccCell, u32, u32) {
    let t = &mut *(own as *mut T1);
    let none = (core::ptr::null_mut(), 0, SHARE);
    let Some(dn) = gravity_abi::breaker::DenseFront::domain(lo, hi) else { return none };
    // the plan's census bounds the domain by the cache cost (`DenseFront::acc_fits`); here the share decides the room
    if topk {
        return none;
    }
    let fb = ((dn as usize * ACC_CELL + 7) & !7) as u32;
    if t.cap != 0 {
        return if t.flags & T1_ACC != 0 { (own.add(t.limit as usize) as *mut AccCell, dn, SHARE) } else { none };
    }
    let share = t.limit;
    if share <= fb + t1_default_bytes() {
        return none;
    }
    let limit = share - fb;
    // the region past the table may be dirty (a share zeroed over its prefix only)
    core::ptr::write_bytes(own.add(limit as usize), 0, fb as usize);
    (own.add(limit as usize) as *mut AccCell, dn, limit)
}

/// # Safety
/// `ctx` is laid out per `programs::par_hashagg`: `partial` at a share-armed region for a
/// `LAYOUT` phase-1 table, `NPARAMS` parameter words.
pub unsafe extern "C-unwind" fn par_hashagg(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let p = params(ctx);
    let own = partial(ctx);
    let (acc, dn, limit) = acc_front(own, p.lo, p.hi, p.n > 0);
    if std::env::var_os("PGROUPDENSE_TRACE").is_some() {
        let t = &*(own as *const T1);
        eprintln!("pgroupdense: lo={} hi={} n={} bits={} ordered={} width={} acc={:?} dn={} limit={} t.cap={} t.flags={} t.limit={}", p.lo, p.hi, p.n, p.bits, p.ordered, (*ctx).width, acc, dn, limit, t.cap, t.flags, t.limit);
    }
    // the partition count: the plan's, floored at 64 for a section of width 8 or more (ruling 12)
    let pbits = gravity_abi::breaker::opened_bits(p.bits, (*ctx).width, false);
    // a raw pointer: the spill host rewrites the pool mid-fold
    let table = T1::at(own, &LAYOUT, pbits, t1_default_bytes(), limit) as *mut T1;
    if !acc.is_null() {
        (*table).flags |= T1_ACC;
    }
    // append from row 0 when the group estimate behind the partition count is past `ADAPT_FACTOR` × the slots (a zone bound keeps the table)
    let est = (1u64 << p.bits) * (512 << 10) / REC as u64;
    if p.n == 0 && est > ADAPT_FACTOR * (*table).cap as u64 {
        (*table).flags |= T1_BYPASS;
    }
    let bypass = (*table).flags & T1_BYPASS != 0;
    let bound = if p.n > 0 { (*table).nth_smallest_key(0, p.n as usize).unwrap_or(i64::MAX) } else { i64::MAX };
    let mut st = Fold { ctx, table, acc, lo: p.lo, dn, nn_live: false, n: p.n, bound, pbits, bypass, last: 0, last_rec: core::ptr::null_mut() };
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            if r == 0 {
                // the claim's end: pending cells fold into the records, the table drains (the zone top-k keeps it: the walk reads its bound)
                if let Err(e) = st.flush_acc() {
                    return e;
                }
                if p.n == 0 {
                    while !T1::drain(table) {
                        if let Err(e) = st.flush() {
                            return e;
                        }
                    }
                }
            }
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, 0);
        let (lk, lv) = (lane(s, 0), lane(s, 1));
        let ok = crate::by_width!(lk.width, group_k(s, lk, lv, &mut st));
        if ok < 0 {
            return ok;
        }
        s.cursor = end(s);
    }
}

/// One worker's fold state for a claim: the phase-1 table, its accumulator, the walk's bound, the bypass.
pub(crate) struct Fold {
    ctx: *mut Ctx,
    table: *mut T1,
    /// `dn` cells for keys `lo..lo + dn` (null: no domain); `nn_live` once a window with a
    /// nullable value lane was seen — before that `nn` is implied by `cnt`.
    acc: *mut AccCell,
    lo: i64,
    dn: u32,
    nn_live: bool,
    n: u32,
    bound: i64,
    pbits: u8,
    bypass: bool,
    last: u64,
    last_rec: *mut u8,
}

impl Fold {
    /// The buffers to the files (the table drained with them); the pool reset.
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

    /// The record of `key`: found or opened at its home pair (the home's occupant evicted into its
    /// buffer on a miss), or appended behind a run collapse (the bypass); a full pool flushes first.
    #[inline(always)]
    unsafe fn record(&mut self, key: i64) -> Result<*mut u8, i64> {
        let keys = [key as u64];
        if self.bypass {
            if !self.last_rec.is_null() && self.last == keys[0] {
                return Ok(self.last_rec);
            }
            let part = KeyHash::of(&keys).part(self.pbits as u32);
            let r = match T1::append_new(self.table, part, &keys) {
                Some(r) => r,
                None => {
                    self.flush()?;
                    T1::append_new(self.table, part, &keys).ok_or(unsupported(UNSUPPORTED_SCRATCH))?
                }
            };
            self.last = keys[0];
            self.last_rec = r;
            return Ok(r);
        }
        let hash = KeyHash::of(&keys);
        if let Some(p) = T1::probe(self.table, &keys, hash) {
            return Ok(p);
        }
        let home = (*self.table).home(hash);
        if !T1::evict(self.table, home) {
            // the flush drained the table: the home is empty now
            self.flush()?;
            return T1::probe(self.table, &keys, hash).ok_or(unsupported(UNSUPPORTED_SCRATCH));
        }
        let r = T1::open_at(self.table, home, &keys);
        if (*self.table).observe() {
            // the record just opened is drained with the rest; it reopens in its buffer
            let mut tmp = [0u8; REC];
            core::ptr::copy_nonoverlapping(r, tmp.as_mut_ptr(), REC);
            self.switch_to_append()?;
            let part = hash.part(self.pbits as u32);
            let dst = match T1::reserve_for(self.table, part) {
                Some(d) => d,
                None => {
                    self.flush()?;
                    T1::reserve_for(self.table, part).ok_or(unsupported(UNSUPPORTED_SCRATCH))?
                }
            };
            core::ptr::copy_nonoverlapping(tmp.as_ptr(), dst, REC);
            return Ok(dst);
        }
        Ok(r)
    }

    /// Every pending accumulator cell into its record, the cell zeroed behind it.
    unsafe fn flush_acc(&mut self) -> Result<(), i64> {
        if self.acc.is_null() {
            return Ok(());
        }
        for d in 0..self.dn as usize {
            let c = self.acc.add(d);
            if (*c).cnt == 0 {
                continue;
            }
            let r = self.record(self.lo + d as i64)?;
            *(r.add(OFF_SUM) as *mut i64) = (*(r.add(OFF_SUM) as *const i64)).wrapping_add((*c).sum);
            *(r.add(OFF_CNT) as *mut i64) = (*(r.add(OFF_CNT) as *const i64)).wrapping_add((*c).cnt);
            let nn = if self.nn_live { (*c).nn } else { (*c).cnt };
            *r.add(OFF_VALID) |= ((nn > 0) as u8) | 2;
            *c = AccCell::default();
        }
        self.nn_live = false;
        Ok(())
    }

    /// The first window with a nullable value lane: `nn` becomes explicit (= `cnt` so far).
    unsafe fn live_nn(&mut self) {
        if self.nn_live {
            return;
        }
        for d in 0..self.dn as usize {
            let c = self.acc.add(d);
            (*c).nn = (*c).cnt;
        }
        self.nn_live = true;
    }
}

#[inline(always)]
pub(crate) unsafe fn group_k<K: Cell>(s: &mut Site, lk: &Lane, lv: &Lane, st: &mut Fold) -> i64 {
    crate::by_width!(lv.width, group::<K>(s, lk, lv, st))
}

/// The dense accumulator's row loop (pass 3 item 1): `sum[key − lo] += v; cnt[key − lo] += 1`
/// over plain cells, no record in the loop. Two lanes without a validity word and no selection
/// vector run the run loop — a key's consecutive rows add in registers and its cell is touched
/// once per run (a cell added per row serialises a sorted layout's long runs on the store →
/// load of one cell: 83 % of the loop's cycles on a sorted input) — anything else the checked
/// loop with `nn` explicit. A key outside the domain goes to its record through `add_outside`.
#[inline(always)]
pub(crate) unsafe fn group_dense<K: Cell, V: Cell>(s: &mut Site, lk: &Lane, lv: &Lane, st: &mut Fold) -> i64 {
    let (kp, vp) = (lk.ptr.ptr() as *const K, lv.ptr.ptr() as *const V);
    let cells = st.acc;
    let (lo, dn) = (st.lo, st.dn as u64);
    let (from, to) = (s.cursor, end(s));
    if s.sel.ptr().is_null() && lk.all_valid() && lv.all_valid() && !st.nn_live {
        // the open run `(rk, rs, rc)`: `rk` is in the domain while `rc > 0` (no int4 key is i64::MIN)
        let (mut rk, mut rs, mut rc) = (i64::MIN, 0i64, 0i64);
        for j in from..to {
            let key = (*kp.add(j as usize)).i32() as i64;
            let v = (*vp.add(j as usize)).i64();
            if key == rk {
                rs = rs.wrapping_add(v);
                rc += 1;
                continue;
            }
            if rc != 0 {
                let c = cells.add(rk.wrapping_sub(lo) as usize);
                (*c).sum = (*c).sum.wrapping_add(rs);
                (*c).cnt += rc;
            }
            let d = key.wrapping_sub(lo) as u64;
            if d < dn {
                (rk, rs, rc) = (key, v, 1);
            } else {
                (rk, rc) = (i64::MIN, 0);
                if let Err(e) = add_outside(st, key, v, true) {
                    s.rows_out += (j - from) as u64;
                    return e;
                }
            }
        }
        if rc != 0 {
            let c = cells.add(rk.wrapping_sub(lo) as usize);
            (*c).sum = (*c).sum.wrapping_add(rs);
            (*c).cnt += rc;
        }
        s.rows_out += (to - from) as u64;
        return 0;
    }
    if !lv.all_valid() {
        st.live_nn();
    }
    let mut out = 0u64;
    for j in from..to {
        let i = crate::util::index(s, j);
        if !lk.is_valid(i) {
            continue;
        }
        let key = (*kp.add(i)).i32() as i64;
        let valid = lv.is_valid(i);
        let v = if valid { (*vp.add(i)).i64() } else { 0 };
        out += 1;
        let d = key.wrapping_sub(lo) as u64;
        if d < dn {
            let c = cells.add(d as usize);
            (*c).sum = (*c).sum.wrapping_add(v);
            (*c).cnt += 1;
            (*c).nn += valid as i64;
        } else if let Err(e) = add_outside(st, key, v, valid) {
            s.rows_out += out;
            return e;
        }
    }
    s.rows_out += out;
    0
}

/// A key outside the dense domain: its record through the hashed path.
#[inline(never)]
pub(crate) unsafe fn add_outside(st: &mut Fold, key: i64, v: i64, valid: bool) -> Result<(), i64> {
    let r = st.record(key)?;
    if valid {
        *(r.add(OFF_SUM) as *mut i64) = (*(r.add(OFF_SUM) as *const i64)).wrapping_add(v);
    }
    *(r.add(OFF_CNT) as *mut i64) = (*(r.add(OFF_CNT) as *const i64)).wrapping_add(1);
    *r.add(OFF_VALID) |= (valid as u8) | 2;
    Ok(())
}

/// The hashed row loop: the record through the phase-1 table's home pair, the sum and count added.
#[inline(always)]
pub(crate) unsafe fn group<K: Cell, V: Cell>(s: &mut Site, lk: &Lane, lv: &Lane, st: &mut Fold) -> i64 {
    if !st.acc.is_null() {
        return group_dense::<K, V>(s, lk, lv, st);
    }
    let (kp, vp) = (lk.ptr.ptr() as *const K, lv.ptr.ptr() as *const V);
    let mut out = 0u64;
    let (from, to) = (s.cursor, end(s));
    for j in from..to {
        let i = crate::util::index(s, j);
        if !lk.is_valid(i) {
            continue;
        }
        let key = (*kp.add(i)).i32() as i64;
        if key > st.bound {
            continue;
        }
        let valid = lv.is_valid(i);
        let v = if valid { (*vp.add(i)).i64() } else { 0 };
        out += 1;
        let r = match st.record(key) {
            Ok(r) => r,
            Err(e) => {
                s.rows_out += out;
                return e;
            }
        };
        if valid {
            *(r.add(OFF_SUM) as *mut i64) = (*(r.add(OFF_SUM) as *const i64)).wrapping_add(v);
        }
        *(r.add(OFF_CNT) as *mut i64) = (*(r.add(OFF_CNT) as *const i64)).wrapping_add(1);
        *r.add(OFF_VALID) |= (valid as u8) | 2;
    }
    s.rows_out += out;
    0
}

// ---- the owner phase (§3.2, §3.4, §3.6) ----

/// The owner functions (the plan's words by value): the zone top-k keeps the `n` smallest keys, any
/// other form its partitions' runs.
pub fn owner_fns(params: &[u64]) -> OwnerFns {
    let mut p = [0u64; OWNER_PARAMS];
    p[..NPARAMS].copy_from_slice(&params[..NPARAMS]);
    OwnerFns { params: p, rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: owner_min, open: owner_open, reduce: owner_reduce, end_round: owner_end, reduce2: owner_reduce, fold: Some(crate::table::owner_fold::owner_fold) }
}

/// The region of a full-emit owner: `OwnerTop` (`worst` = the runs, `total` = the run bytes, `len` =
/// the records), the run directory `(offset, records)` per run, the run area up to `table_off`, the
/// table space; a top-k owner's: `OwnerTop`, `n` kept records, `u32[n]` heap indexes, the table space.
pub(crate) const DIR_OFF: usize = OWNERTOP_HDR;

pub(crate) const RUN_OFF: usize = DIR_OFF + 8 * MAX_RUNS;

pub(crate) const fn topk_table_off(n: usize) -> usize {
    (OWNERTOP_HDR + n * REC + 4 * n + 7) & !7
}

pub(crate) unsafe fn owner_params(params: *const u64) -> Params {
    params_of(core::slice::from_raw_parts(params, NPARAMS))
}

pub(crate) fn owner_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `NPARAMS` plan words.
    let p = unsafe { owner_params(params) };
    let table = HASHAGG_HDR + 64 * REC;
    if p.n > 0 {
        return (topk_table_off(p.n as usize) + table) as u64;
    }
    // the run area must hold this owner's share of the groups the partition count implies (a half of the region)
    let est = (1u64 << p.bits) * (512 << 10) / REC as u64;
    RUN_OFF as u64 + (est * REC as u64).max(2 * table as u64)
}

pub(crate) unsafe fn owner_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let p = owner_params(params);
    let table_off = if p.n > 0 { topk_table_off(p.n as usize) } else { RUN_OFF + ((bytes.saturating_sub(RUN_OFF) / 2) & !7) };
    if table_off + HASHAGG_HDR + 2 * REC > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: p.n, table_off: table_off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// A folded partition: its records into the owner's top (the zone top-k) or as one run (sorted by
/// key under the order); the table is the owner's scratch.
pub(crate) unsafe fn owner_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let p = owner_params(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    h.groups += t.len as u64;
    h.records += t.len as u64;
    if p.n > 0 {
        let mut top = Top { base: owner.add(OWNERTOP_HDR), heap: owner.add(OWNERTOP_HDR + p.n as usize * REC) as *mut u32, n: p.n as usize, len: h.len as usize };
        for i in 0..t.cap as usize {
            let r = t.record(i);
            if r.used() {
                top.offer(r.as_ptr());
            }
        }
        h.len = top.len as u32;
        return 0;
    }
    if p.ordered {
        t.sort_dense_by_key(0);
    }
    let n = t.len as usize;
    if n == 0 {
        return 0;
    }
    let runs = h.worst as usize;
    let off = RUN_OFF + h.total as usize;
    if runs >= MAX_RUNS || off + n * REC > h.table_off as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    // a sorted table is dense at 0..n; an unsorted one is compacted here
    let mut m = 0usize;
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if r.used() {
            core::ptr::copy_nonoverlapping(r.as_ptr(), owner.add(off + m * REC), REC);
            m += 1;
        }
    }
    let dir = owner.add(DIR_OFF) as *mut u32;
    *dir.add(2 * runs) = off as u32;
    *dir.add(2 * runs + 1) = m as u32;
    h.worst = runs as u32 + 1;
    h.total += (m * REC) as u64;
    h.len += m as u32;
    0
}

/// After the owner's last partition: a top-k owner's index heap sorted key-ascending.
pub(crate) unsafe fn owner_end(params: *const u64, owner: *mut u8) -> i64 {
    let p = owner_params(params);
    if p.n == 0 {
        return 0;
    }
    let h = &*(owner as *const OwnerTop);
    Top { base: owner.add(OWNERTOP_HDR), heap: owner.add(OWNERTOP_HDR + p.n as usize * REC) as *mut u32, n: p.n as usize, len: h.len as usize }.sort();
    0
}

#[inline(always)]
pub(crate) unsafe fn key_at(p: *const u8) -> i64 {
    *(p as *const i64)
}

/// The `n` smallest keys so far: kept copies over `base`, an index heap whose root is the largest kept key.
pub(crate) struct Top {
    base: *mut u8,
    heap: *mut u32,
    n: usize,
    len: usize,
}

impl Top {
    unsafe fn at(&self, i: usize) -> *mut u8 {
        self.base.add(i * REC)
    }

    unsafe fn key(&self, hi: usize) -> i64 {
        key_at(self.at(*self.heap.add(hi) as usize))
    }

    unsafe fn offer(&mut self, r: *const u8) {
        let slot = if self.len < self.n {
            self.len
        } else {
            if key_at(r) >= self.key(0) {
                return;
            }
            *self.heap as usize
        };
        core::ptr::copy_nonoverlapping(r, self.at(slot), REC);
        if slot == self.len {
            *self.heap.add(self.len) = slot as u32;
            self.len += 1;
            self.sift_up(self.len - 1);
        } else {
            self.sift_down(0, self.len);
        }
    }

    unsafe fn swap(&mut self, i: usize, j: usize) {
        core::ptr::swap(self.heap.add(i), self.heap.add(j));
    }

    unsafe fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.key(i) <= self.key(parent) {
                break;
            }
            self.swap(i, parent);
            i = parent;
        }
    }

    unsafe fn sift_down(&mut self, mut i: usize, end: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < end && self.key(l) > self.key(m) {
                m = l;
            }
            if r < end && self.key(r) > self.key(m) {
                m = r;
            }
            if m == i {
                return;
            }
            self.swap(i, m);
            i = m;
        }
    }

    /// The index heap into key-ascending order (`heap[0..len]`).
    unsafe fn sort(&mut self) {
        for end in (1..self.len).rev() {
            self.swap(0, end);
            self.sift_down(0, end);
        }
    }
}

// ---- the emit ----

/// # Safety
/// `ctx` is laid out per `programs::par_hashagg` with `partial` at the owners' results (`TABLE_OWNED`:
/// the `W` owners' or the width-1 arm's one).
pub unsafe extern "C-unwind" fn par_hashagg_emit(ctx: *mut Ctx) -> i64 {
    let word = (*ctx).resume;
    let p = params(ctx);
    let own = partial(ctx);
    if table_kind(own) != TABLE_OWNED {
        return unsupported(UNSUPPORTED_SHAPE);
    }
    emit_owned(ctx, own, &p, word)
}

/// One group row: `(k, sum, count)`; `HOST_STOP` when the sink said stop, else `HOST_CONTINUE` (an error aborts).
#[inline(always)]
pub(crate) unsafe fn emit_record(ctx: *mut Ctx, r: &Rec<'_>) -> i64 {
    let v = [r.key(0), int8_datum(r.i64(0)), int8_datum(r.count(1))];
    let n = [0u8, !r.valid(0) as u8, 0];
    let f = emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)]);
    if f < 0 {
        return ERROR;
    }
    f
}

/// The owners' results (§3.4, §3.6): the zone top-k merges the `W` sorted tops and emits `n`; an
/// ordered form k-way merges every owner's runs (its heap and positions in owner 0's table space,
/// so a stop resumes it); an unordered one drains the runs in order, resumable at `(run, record)`.
pub(crate) unsafe fn emit_owned(ctx: *mut Ctx, own: *mut u8, p: &Params, word: i64) -> i64 {
    let o = Owned::view(own);
    let w = o.nowners as usize;
    if w > MAX_OWNERS {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    if p.n > 0 {
        let mut pos = [0usize; MAX_OWNERS];
        for _ in 0..p.n as usize {
            let (mut best, mut bk) = (usize::MAX, i64::MAX);
            for (j, pj) in pos.iter().enumerate().take(w) {
                let h = OwnerTop::view(o.owner(j));
                if *pj >= h.len as usize {
                    continue;
                }
                let heap = h.records().add(p.n as usize * REC) as *const u32;
                let k = key_at(h.records().add(*heap.add(*pj) as usize * REC));
                if best == usize::MAX || k < bk {
                    best = j;
                    bk = k;
                }
            }
            if best == usize::MAX {
                break;
            }
            let h = OwnerTop::view(o.owner(best));
            let heap = h.records().add(p.n as usize * REC) as *const u32;
            let r = Rec::at(h.records().add(*heap.add(pos[best]) as usize * REC), &LAYOUT);
            pos[best] += 1;
            if emit_record(ctx, &r) < 0 {
                return ERROR;
            }
        }
        (*ctx).resume = RESUME_NONE;
        return if flush(ctx) < 0 { ERROR } else { DONE };
    }
    // the runs, numbered across the owners in owner order
    let mut nruns = 0usize;
    for j in 0..w {
        nruns += OwnerTop::view(o.owner(j)).worst as usize;
    }
    let run = |g: usize| -> (*mut u8, usize) {
        let mut g = g;
        for j in 0..w {
            let h = OwnerTop::view(o.owner(j));
            if g < h.worst as usize {
                let dir = h.records().add(DIR_OFF - OWNERTOP_HDR) as *const u32;
                return (o.owner(j).add(*dir.add(2 * g) as usize), *dir.add(2 * g + 1) as usize);
            }
            g -= h.worst as usize;
        }
        (core::ptr::null_mut(), 0)
    };
    if !p.ordered {
        let (mut g, mut i) = if word == RESUME_NONE { (0usize, 0usize) } else { ((resume::position(word) >> 24) as usize, (resume::position(word) & 0xff_ffff) as usize) };
        while g < nruns {
            let (base, len) = run(g);
            while i < len {
                let r = Rec::at(base.add(i * REC), &LAYOUT);
                i += 1;
                if emit_record(ctx, &r) == HOST_STOP {
                    (*ctx).resume = resume::word(resume::EMIT, ((g as u64) << 24) | i as u64);
                    return suspend(crate::util::out_count(ctx) == 0);
                }
            }
            g += 1;
            i = 0;
        }
        (*ctx).resume = RESUME_NONE;
        return if flush(ctx) < 0 { ERROR } else { DONE };
    }
    // the k-way merge's state: `heap_len`, then `heap[nruns]` and `pos[nruns]` in owner 0's table space
    let h0 = OwnerTop::view(o.owner(0));
    let state = o.owner(0).add(h0.table_off as usize) as *mut u32;
    if h0.table_off as usize + 4 + 8 * nruns > o.owner_bytes as usize {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let heap = state.add(1);
    let pos = heap.add(nruns);
    let head = move |g: usize| -> i64 {
        let (base, _) = run(g);
        key_at(base.add(*pos.add(g) as usize * REC))
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
        let r = Rec::at(base.add(*pos.add(g) as usize * REC), &LAYOUT);
        *pos.add(g) += 1;
        if *pos.add(g) as usize >= len {
            *state -= 1;
            *heap = *heap.add(*state as usize);
        }
        sift_down(*state as usize, 0);
        if emit_record(ctx, &r) == HOST_STOP {
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
