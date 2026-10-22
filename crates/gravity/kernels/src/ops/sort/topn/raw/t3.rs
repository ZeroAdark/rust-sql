//! The tier-3 record plan's raw half (ops/sort/topn/t3.rs keeps the plan words, `Plan`, its layouts and the owner
//! tables `owner_fns_pass1` / `owner_fns_distinct` hand out): the plan read off a context, the record readers, the bare
//! pass 1, the export and distinct owner phases with their pair folds, and the round-2 top. Moved unchanged from t3.rs
//! (rule R6: the family's unsafe code lives in raw); t3.rs re-exports the public functions under their old paths.
use gravity_abi::PtrWord;
use gravity_abi::breaker::{HashAgg, Owned, OwnerTop, Rec, RecordLayout, SubSplit, T1, HASHAGG_HDR, MOVING, OWNERTOP_HDR, REDUCE_FLUSH};
use gravity_abi::hash::KeyHash;
use gravity_abi::program::consumed;
use gravity_abi::site::dict_value;
use gravity_abi::status::{unsupported, ERROR, UNSUPPORTED_DOMAIN, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
use gravity_abi::t3::{ROUND2_T1_BYTES, ROUND2_T1_MAX};
use gravity_abi::{Ctx, Site};

use core::sync::atomic::Ordering;

use crate::ops::sort::topn::t3_dense;
use crate::ops::sort::topn::t3::{distinct_geometry, distinct_list_off, export_list, export_rec, grouped, is_text, Loop, PairFold, Plan, BARE_INIT, CENSUS_ON, EXPORT_DUPS, EXPORT_TOP, GMARK_ENTRY, K_CODE, K_TEXT, MAX_KEYS, MODE_TWO_PASS, NPARAMS, ORDER_COUNT_DESC, P1_BASE, PF_AVG, PF_COUNT, PF_ONE, PF_SUM, P_SHAPE, S_AVG, S_AVG_TBL, S_MIN_CODE, S_SUM};

impl Plan {
    /// # Safety — `ctx` carries `NPARAMS` parameter words.
    pub unsafe fn read(ctx: *mut Ctx) -> Option<Plan> {
        let p = core::slice::from_raw_parts((*ctx).params.ptr(), NPARAMS);
        if p[P_SHAPE] >> 24 & MODE_TWO_PASS as u64 == 0 {
            return Plan::from_words(p);
        }
        // the two-pass form: pass 1 (no consumed slot) reads its own block; pass 2 and the emit this one
        if consumed(ctx, 0).is_null() {
            return Plan::from_words(core::slice::from_raw_parts((*ctx).params.ptr().add(P1_BASE), NPARAMS)).map(|mut pl| {
                pl.two_pass = true;
                pl
            });
        }
        Plan::from_words(p).map(|mut pl| {
            pl.two_pass = true;
            pl.admit = true;
            pl.site = 1;
            pl
        })
    }

    /// Key `k`'s word for ordering and output: an int key's word, or a text key's representative
    /// slot (`base` slots before it) — the rendered datum once the leader rendered it.
    #[inline(always)]
    pub unsafe fn key_out(&self, r: &Rec<'_>, k: usize, base: usize) -> u64 {
        if is_text(self.key_kind[k]) { slot_word(r, base + self.rep_slot[k]) } else { r.key(self.key_word[k]) }
    }
}

/// The bare owner table's dup area behind its slots: `(zeros, ndups, dups, dup capacity)` — initialised once per table.
unsafe fn bare_area(t: &mut HashAgg) -> (*mut u64, *mut u64, *mut u64, usize) {
    let words = (t.limit as usize).saturating_sub(HASHAGG_HDR) / 8;
    let base = t.base_ptr() as *mut u64;
    let area = base.add(t.cap as usize);
    if t.flags & BARE_INIT == 0 {
        *area = 0;
        *area.add(1) = 0;
        t.flags |= BARE_INIT;
    }
    (area, area.add(1), area.add(2), words.saturating_sub(t.cap as usize + 2))
}

/// The bare form's owner fold: every word into the table's open-addressing set (a slot holds a key;
pub(in crate::ops::sort::topn) unsafe fn bare_fold(_params: *const u64, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, _cand: *mut gravity_abi::breaker::Candidates) -> i64 {
    let t = &mut *t;
    let m = if t.cap.is_power_of_two() { t.cap as usize } else { (t.cap as usize).next_power_of_two() / 2 };
    let m = m.max(2);
    let (cap_bits, mask) = (m.trailing_zeros(), m - 1);
    // V4R-197: the home reads the hash bits below the route the split consumed (`KeyHash::home`, as every keyed table since
    // D250): the partition's keys share its top `pbits`, so the top bits homed a partition into 1 / 2^pbits of the slots
    let at = split.table_depth();
    let slots = t.base_ptr() as *mut u64;
    let (zeros, ndups, dups, dcap) = bare_area(t);
    let words = recs as *const u64;
    for r in 0..nrecs {
        let k = *words.add(r);
        if split.depth != 0 && !split.keeps(KeyHash::of(&[k])) {
            continue;
        }
        let again = if k == 0 {
            *zeros += 1;
            *zeros == 2
        } else {
            if t.len as usize >= m - m / 8 {
                return -1 - r as i64;
            }
            let mut pos = KeyHash::word(k).home(cap_bits, at);
            loop {
                let v = *slots.add(pos);
                if v == 0 {
                    *slots.add(pos) = k;
                    t.len += 1;
                    break false;
                }
                if v == k {
                    break true;
                }
                pos = (pos + 1) & mask;
            }
        };
        if again {
            if *ndups as usize >= dcap {
                return -1 - r as i64;
            }
            *dups.add(*ndups as usize) = k;
            *ndups += 1;
        }
    }
    nrecs as i64
}

/// The bare form's reduce: the dup area's keys to the owner's list (repeats included: the admitted set is a bitmap).
pub(in crate::ops::sort::topn) unsafe fn bare_reduce(_params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let t = &mut *(table as *mut HashAgg);
    let h = &mut *(owner as *mut OwnerTop);
    h.groups += t.len as u64;
    h.records += t.len as u64;
    let (_, ndups, dups, _) = bare_area(t);
    for i in 0..*ndups as usize {
        let Some(d) = export_slot(owner, h, 8) else { return unsupported(UNSUPPORTED_SCRATCH) };
        *(d as *mut u64) = *dups.add(i);
    }
    0
}

pub(in crate::ops::sort::topn) fn export_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `NPARAMS` plan words.
    let Some(pl) = Plan::from_words(unsafe { core::slice::from_raw_parts(params, NPARAMS) }) else { return u64::MAX };
    let rec = export_rec(&pl) as u64;
    8 * (OWNERTOP_HDR as u64 + HASHAGG_HDR as u64 + 64 * rec)
}

pub(in crate::ops::sort::topn) unsafe fn export_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let Some(pl) = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)) else { return unsupported(UNSUPPORTED_SHAPE) };
    let rec = export_rec(&pl);
    let (list_off, list_bytes) = export_list(bytes);
    let table_off = list_off + list_bytes;
    if table_off + HASHAGG_HDR + 2 * rec > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let thr = if pl.export & 0xff == EXPORT_DUPS { 2 } else { 0 };
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: 0, table_off: table_off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// The list's next record slot, or none when the quarter is full.
#[inline(always)]
unsafe fn export_slot(owner: *mut u8, h: &mut OwnerTop, rec: usize) -> Option<*mut u8> {
    let (list_off, _) = export_list(0);
    let cap = (h.table_off as usize - list_off) / rec;
    if h.total as usize >= cap {
        return None;
    }
    let p = owner.add(list_off + h.total as usize * rec);
    h.total += 1;
    Some(p)
}

/// Every folded record through the rule to the list, whole: the driver's candidates (count ≥ 2,
pub(in crate::ops::sort::topn) unsafe fn export_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    let Some(pl) = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)) else { return unsupported(UNSUPPORTED_SHAPE) };
    let layout = pl.layout();
    let h = &mut *(owner as *mut OwnerTop);
    h.groups += (*table).len as u64;
    h.records += (*table).len as u64;
    let rec = layout.size() as usize;
    let (rule, thr) = (pl.export & 0xff, (pl.export >> 8) as i64);
    let t = &*table;
    let (off_used, off_count) = (t.layout.sentinel_off as usize, t.layout.slot_off[0] as usize);
    let base = t.base_ptr();
    let mut keep = |p: *mut u8| -> Result<(), i64> {
        let w = p.add(off_count) as *mut u64;
        *w &= !MOVING;
        let c = *w as i64;
        if if rule == EXPORT_DUPS { c >= 2 } else { c > thr } {
            let Some(d) = export_slot(owner, h, rec) else { return Err(unsupported(UNSUPPORTED_SCRATCH)) };
            core::ptr::copy_nonoverlapping(p, d, rec);
        }
        Ok(())
    };
    if ncand >= 0 {
        for k in 0..ncand as usize {
            if let Err(e) = keep(base.add(*cand.add(k) as usize * rec)) {
                return e;
            }
        }
        return 0;
    }
    for i in 0..t.cap as usize {
        let p = base.add(i * rec);
        if *(p.add(off_used) as *const u64) == 0 {
            continue;
        }
        if let Err(e) = keep(p) {
            return e;
        }
    }
    0
}

/// A record's text representatives rendered in place (`(part, code)` → the datum resident with the part).
pub unsafe fn render_reps(pl: &Plan, layout: &RecordLayout, site: *mut Site, r: *mut u8) -> i64 {
    let mut one = |slot: usize, lane: usize| -> i64 {
        let off = r.add(layout.slot_off[slot] as usize) as *mut u64;
        let raw = *off;
        let mut v = 0u64;
        if raw == u64::MAX || dict_value(site, (raw >> 32) as u32, lane as u32, raw as u32, &mut v) < 0 {
            return ERROR;
        }
        *off = v;
        0
    };
    for k in 0..pl.nkeys {
        if is_text(pl.key_kind[k]) && one(pl.rep_slot[k], pl.key_lane[k]) < 0 {
            return ERROR;
        }
    }
    for sj in 1..pl.nslots {
        if pl.slot_kind[sj] == S_MIN_CODE && one(sj, pl.slot_lane[sj]) < 0 {
            return ERROR;
        }
    }
    0
}

/// Every record of a finished export region (`owner_fns_pass1` / `owner_fns_pass2`'s layout): the
pub unsafe fn export_records(owned: *const u8, p: &[u64], f: &mut dyn FnMut(*const u8)) -> Option<usize> {
    let pl = Plan::from_words(p)?;
    let o = Owned::view(owned);
    let mut n = 0usize;
    if pl.export & 0xff == EXPORT_TOP {
        // the distinct family's tops (the accumulator layout, the text keys rendered by the owners)
        let rec = pl.layout_of(pl.nkeys - 1, 1).size() as usize;
        for i in 0..o.nowners as usize {
            let h = OwnerTop::view(o.owner(i));
            for j in 0..h.len as usize {
                f(h.records().add(j * rec));
            }
            n += h.len as usize;
        }
        return Some(n);
    }
    let rec = export_rec(&pl);
    for i in 0..o.nowners as usize {
        let h = OwnerTop::view(o.owner(i));
        let list = o.owner(i).add(export_list(0).0);
        for j in 0..h.total as usize {
            f(list.add(j * rec));
        }
        n += h.total as usize;
    }
    Some(n)
}

/// The granule marks of a finished distinct pass-1 region (`MISC_GMARK`): every owner's listed
pub unsafe fn gmark_lists(owned: *const u8, p1: &[u64], f: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    let pl = Plan::from_words(p1)?;
    if !pl.gmark || pl.export & 0xff != EXPORT_TOP {
        return None;
    }
    let o = Owned::view(owned);
    let list_off = distinct_list_off(&pl);
    let mut n = 0usize;
    for i in 0..o.nowners as usize {
        let h = OwnerTop::view(o.owner(i));
        if h.total == u64::MAX {
            return None;
        }
        let list = o.owner(i).add(list_off);
        for j in 0..h.total as usize {
            let e = list.add(j * GMARK_ENTRY) as *const u64;
            f(*e, *e.add(1), *e.add(2));
        }
        n += h.total as usize;
    }
    Some(n)
}

/// The exported keys of a finished pass-1 region: every record's first key word, to `f`.
pub unsafe fn export_keys(owned: *const u8, p1: &[u64], f: &mut dyn FnMut(u64)) -> Option<usize> {
    let key_off = Plan::from_words(p1)?.layout().key_off(0);
    export_records(owned, p1, &mut |r| f(*(r.add(key_off) as *const u64)))
}

pub(in crate::ops::sort::topn) fn distinct_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `NPARAMS` plan words.
    let Some(pl) = Plan::from_words(unsafe { core::slice::from_raw_parts(params, NPARAMS) }) else { return u64::MAX };
    let acc = pl.layout_of(pl.nkeys - 1, 1).size() as u64;
    // the round-2 table scales to the region (`distinct_open`): the floor is the results, a 4 KiB round-2 region and a 64-record table
    OWNERTOP_HDR as u64 + (pl.limit + pl.offset) as u64 * acc + 2 * 4096 + HASHAGG_HDR as u64 + 64 * pl.layout().size() as u64
}

pub(in crate::ops::sort::topn) unsafe fn owner_noop(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

pub(in crate::ops::sort::topn) unsafe fn distinct_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let Some(pl) = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)) else { return unsupported(UNSUPPORTED_SHAPE) };
    let ng = pl.nkeys - 1;
    let acc = pl.layout_of(ng, 1);
    let n = if ng == 0 { 0 } else { pl.limit + pl.offset };
    let (list_off, round2_off, table_off) = distinct_geometry(&pl, bytes);
    let r2_bytes = table_off - round2_off;
    if ng == 0 || r2_bytes < 4096 || table_off + HASHAGG_HDR + 2 * pl.layout().size() as usize > bytes {
        if ng != 0 {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        // the plain count: no round-2 buffers, the table space is the rest
        *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: 0, table_off: list_off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
        return list_off as i64;
    }
    let pbits2 = crate::table::params::spill_bits(pl.groups, acc.size()).max(3);
    let r2 = owner.add(round2_off);
    core::ptr::write_bytes(r2, 0, 64);
    // the hot table: two slots per expected group in the pair mode (every partition touches most groups),
    let want = if grouped(&pl, Loop::of(&pl)) { 4096 } else { (2 * pl.groups * acc.size() as u64).clamp(ROUND2_T1_BYTES as u64, ROUND2_T1_MAX as u64).next_power_of_two() as u32 };
    T1::at(r2, &acc, pbits2, want.min(r2_bytes as u32 / 4), r2_bytes as u32);
    // the dense area before the partition table: the owner fold counts at insert (`fold::owner_fold_dense`) when it fits the table space
    let table_off = table_off + t3_dense::Dense::lay(params, owner.add(table_off), bytes - table_off);
    if table_off + HASHAGG_HDR + 2 * pl.layout().size() as usize > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: n as u32, table_off: table_off as u32, worst: 0, round2_off: round2_off as u32, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    table_off as i64
}

/// Round 1: the partition's deduped pairs counted per group into the owner's round-2 table; a full
pub(in crate::ops::sort::topn) unsafe fn distinct_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let Some(pl) = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)) else { return unsupported(UNSUPPORTED_SHAPE) };
    let ng = pl.nkeys - 1;
    let h = &mut *(owner as *mut OwnerTop);
    let t = &*table;
    if h.resume == 0 {
        h.records += t.len as u64;
    }
    if ng == 0 {
        h.total += t.len as u64;
        return 0;
    }
    let acc = pl.layout_of(ng, 1);
    let r2 = owner.add(h.round2_off as usize) as *mut T1;
    let nw = pl.key_word[ng];
    if !pl.gmark && !acc.has_validity() && !t.layout.has_validity() {
        if let Some(d) = t3_dense::Dense::at_table(params, t) {
            // D597: a key outside the domain was kept out of the table: refused by name, never a count short
            if d.bad() {
                return unsupported(UNSUPPORTED_DOMAIN);
            }
            // the fold counted this table at the insert (it holds identities: nothing to walk); the next table's stream starts at 0
            d.end_stream();
            h.resume = 0;
            return 0;
        }
        let f = PairFold::of(&pl, &acc, &t.layout, ng);
        return if grouped(&pl, Loop::of(&pl)) { reduce_grouped(h, t, r2, &f, nw) } else { reduce_pairs(h, t, r2, &f, nw) };
    }
    let list_off = distinct_list_off(&pl);
    let mark = pl.nwords - 1;
    for i in h.resume as usize..t.cap as usize {
        let r = t.record(i);
        if !r.used() {
            continue;
        }
        if pl.gmark && r.key(mark) != 0 {
            // a phantom: listed for the leader, never a pair
            let list = owner.add(list_off);
            let room = (h.round2_off as usize - list_off) / GMARK_ENTRY;
            if h.total as usize >= room {
                h.total = u64::MAX;
            }
            if h.total != u64::MAX {
                let e = list.add(h.total as usize * GMARK_ENTRY) as *mut u64;
                *e = r.key(0);
                *e.add(1) = r.key(nw);
                *e.add(2) = r.count(0) as u64 & !MOVING;
                h.total += 1;
            }
            continue;
        }
        let keys = &r.keys()[..nw];
        let hash = KeyHash::of(keys);
        let p = match T1::probe(r2, keys, hash) {
            Some(p) => p,
            None => {
                let home = (*r2).home(hash);
                if !T1::evict(r2, home) {
                    h.resume = i as u32;
                    return REDUCE_FLUSH;
                }
                T1::open_at(r2, home, keys)
            }
        };
        fold_pair(&pl, &Rec::at(p, &acc), &r, ng);
    }
    h.resume = 0;
    0
}

impl PairFold {
    /// `src`'s pair folded into the accumulator record at `dst` (both word pointers).
    #[inline(always)]
    pub(crate) unsafe fn fold(&self, dst: *mut u64, src: *const u64) {
        self.fold_from(0, dst, src)
    }

    #[inline(always)]
    unsafe fn fold_from(&self, first: usize, dst: *mut u64, src: *const u64) {
        for e in first..self.n {
            let (d, s) = (dst.add(self.dst[e]), src.add(self.src[e]));
            match self.kind[e] {
                PF_ONE => *d += 1,
                PF_COUNT => *d = (*d).wrapping_add(*s & !MOVING),
                PF_SUM => *d = (*d).wrapping_add(*s),
                PF_AVG => {
                    let v = ((*d as u128) | (*d.add(1) as u128) << 64).wrapping_add((*s as u128) | (*s.add(1) as u128) << 64);
                    *d = v as u64;
                    *d.add(1) = (v >> 64) as u64;
                    *d.add(2) = (*d.add(2)).wrapping_add(*s.add(2));
                }
                _ => {
                    if *s < *d {
                        *d = *s;
                    }
                }
            }
        }
    }

    /// The accumulator image of one pair (its group words, the count 1, its slots, its representatives) at `dst`.
    #[inline(always)]
    unsafe fn image(&self, dst: *mut u64, src: *const u64, nw: usize) {
        let mut w = 0;
        while w < nw {
            *dst.add(w) = *src.add(w);
            w += 1;
        }
        for e in 0..self.n {
            let (d, s) = (dst.add(self.dst[e]), src.add(self.src[e]));
            match self.kind[e] {
                PF_ONE => *d = 1,
                PF_COUNT => *d = *s & !MOVING,
                PF_AVG => {
                    *d = *s;
                    *d.add(1) = *s.add(1);
                    *d.add(2) = *s.add(2);
                }
                _ => *d = *s,
            }
        }
    }
}

/// The pair mode: every deduped pair probed into the round-2 table (the hot groups; an evicted
unsafe fn reduce_pairs(h: &mut OwnerTop, t: &HashAgg, r2: *mut T1, f: &PairFold, nw: usize) -> i64 {
    let (rec, sent, base) = (t.layout.size as usize, t.layout.sentinel_off as usize, t.base_ptr());
    for i in h.resume as usize..t.cap as usize {
        let p = base.add(i * rec);
        if *(p.add(sent) as *const u64) == 0 {
            continue;
        }
        let keys = core::slice::from_raw_parts(p as *const u64, nw);
        let hash = KeyHash::of(keys);
        let d = match T1::probe(r2, keys, hash) {
            Some(d) => d,
            None => {
                let home = (*r2).home(hash);
                if !T1::evict(r2, home) {
                    h.resume = i as u32;
                    return REDUCE_FLUSH;
                }
                T1::open_at(r2, home, keys)
            }
        };
        f.fold(d as *mut u64, p as *const u64);
    }
    h.resume = 0;
    0
}

/// The grouped mode: a group's pairs all lie in this partition, so every deduped pair's group record
unsafe fn reduce_grouped(h: &mut OwnerTop, t: &HashAgg, r2: *mut T1, f: &PairFold, nw: usize) -> i64 {
    let (rec, sent, base) = (t.layout.size as usize, t.layout.sentinel_off as usize, t.base_ptr());
    let pbits = (*r2).pbits;
    let mut ap = T1::appender(r2);
    for i in h.resume as usize..t.cap as usize {
        let p = base.add(i * rec);
        if *(p.add(sent) as *const u64) == 0 {
            continue;
        }
        let keys = core::slice::from_raw_parts(p as *const u64, nw);
        let part = KeyHash::of(keys).part(pbits as u32);
        let Some(d) = ap.reserve(part) else {
            ap.finish();
            h.resume = i as u32;
            return REDUCE_FLUSH;
        };
        f.image(d as *mut u64, p as *const u64, nw);
    }
    ap.finish();
    h.resume = 0;
    0
}

pub(in crate::ops::sort::topn) unsafe fn distinct_end_round(params: *const u64, owner: *mut u8) -> i64 {
    let h = &*(owner as *const OwnerTop);
    if h.round2_off == 0 {
        return 0;
    }
    let r2 = owner.add(h.round2_off as usize) as *mut T1;
    let mut dense = 0;
    if h.records != 0 {
        let t = owner.add(h.table_off as usize) as *const HashAgg;
        if let Some(d) = t3_dense::Dense::at_table(params, t) {
            let rc = d.append(r2);
            if rc != 0 {
                return rc;
            }
            dense = d.len();
        }
    }
    if CENSUS_ON.load(Ordering::Relaxed) {
        let t = &*r2;
        let groups = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)).map_or(0, |pl| pl.groups);
        eprintln!("DISTINCT owner round 1: pairs={} r2 cap={} len={} inserts={} evictions={} buffered={} groups_est={} dense={dense}", h.records, t.cap, t.len, t.inserts, t.evictions, t.buffered, groups); // alloc: Cold
    }
    if !T1::drain(r2) {
        return REDUCE_FLUSH;
    }
    0
}

/// Round 2: a partition's group records folded across the owners, the best kept under the order.
pub(in crate::ops::sort::topn) unsafe fn distinct_reduce2(params: *const u64, owner: *mut u8, table: *const HashAgg, site: *mut Site, cand: *const u32, ncand: i64) -> i64 {
    let Some(pl) = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)) else { return unsupported(UNSUPPORTED_SHAPE) };
    let ng = pl.nkeys - 1;
    let acc = pl.layout_of(ng, 1);
    let text = pl.key_kind[..ng].contains(&K_TEXT);
    let h = &mut *(owner as *mut OwnerTop);
    h.groups += (*table).len as u64;
    let mut top = Top { base: h.records(), rec: acc.size() as usize, n: h.n as usize, len: h.len as usize, worst: h.worst as usize, pl: &pl, layout: &acc, nkeys: ng, slot_base: 1, rendered: text };
    if let Err(e) = top.offer_folded(site, text, &*table, ng, cand, ncand) {
        return e;
    }
    h.len = top.len as u32;
    h.worst = top.worst as u32;
    h.thr = top.threshold();
    0
}

/// The `n` best records so far; `worst` is the index a new record must beat.
struct Top<'a> {
    base: *mut u8,
    rec: usize,
    n: usize,
    len: usize,
    worst: usize,
    pl: &'a Plan,
    layout: &'a RecordLayout,
    nkeys: usize,
    /// Slots before the plain ones (the accumulator's pair count); whether the text words are rendered.
    slot_base: usize,
    rendered: bool,
}

unsafe fn text_less(a: u64, b: u64) -> core::cmp::Ordering {
    let bytes = |d: u64| core::slice::from_raw_parts((d as *const u8).add(4), (*(d as *const u32) >> 2) as usize - 4);
    bytes(a).cmp(&bytes(b))
}

impl Top<'_> {
    unsafe fn at(&self, i: usize) -> *const u8 {
        self.base.add(i * self.rec)
    }

    /// Whether the record at `a` ranks before the one at `b` (the query's ORDER BY, ties by the keys).
    unsafe fn better(&self, a: *const u8, b: *const u8) -> bool {
        let (ra, rb) = (Rec::at(a as *mut u8, self.layout), Rec::at(b as *mut u8, self.layout));
        if self.pl.order == ORDER_COUNT_DESC {
            let (ca, cb) = (ra.count(self.pl.order_slot), rb.count(self.pl.order_slot));
            if ca != cb {
                return ca > cb;
            }
        }
        for k in 0..self.nkeys {
            let (wa, wb) = (self.pl.key_out(&ra, k, self.slot_base), self.pl.key_out(&rb, k, self.slot_base));
            if wa == wb {
                continue;
            }
            let text = self.rendered && (self.pl.key_kind[k] == K_CODE || self.pl.key_kind[k] == K_TEXT);
            return if text { text_less(wa, wb) == core::cmp::Ordering::Less } else { (wa as i64) < (wb as i64) };
        }
        false
    }

    unsafe fn find_worst(&mut self) {
        let mut w = 0;
        for i in 1..self.len {
            if self.better(self.at(w), self.at(i)) {
                w = i;
            }
        }
        self.worst = w;
    }

    unsafe fn offer(&mut self, r: *const u8) {
        if self.len < self.n {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.len * self.rec), self.rec);
            self.len += 1;
            if self.len == self.n {
                self.find_worst();
            }
            return;
        }
        if self.better(r, self.at(self.worst)) {
            core::ptr::copy_nonoverlapping(r, self.base.add(self.worst * self.rec), self.rec);
            self.find_worst();
        }
    }

    /// The count a candidate must reach to enter (the worst kept's once `n` are kept; 0 under a key order).
    #[inline(always)]
    unsafe fn threshold(&self) -> i64 {
        if self.pl.order != ORDER_COUNT_DESC || self.len < self.n {
            return 0;
        }
        Rec::at(self.at(self.worst) as *mut u8, self.layout).count(self.pl.order_slot)
    }

    /// A folded partition's records to the top: the driver's candidates (their `MOVING` mark
    unsafe fn offer_folded(&mut self, s: *mut Site, text: bool, t: &HashAgg, ng: usize, cand: *const u32, ncand: i64) -> Result<(), i64> {
        if ncand < 0 {
            return if text { self.offer_table_lazy(s, t, ng) } else { self.offer_table(t); Ok(()) };
        }
        let (off_count, rec) = (t.layout.slot_off[self.pl.order_slot] as usize, t.layout.size as usize);
        let base = t.base_ptr();
        for k in 0..ncand as usize {
            let p = base.add(*cand.add(k) as usize * rec);
            let w = p.add(off_count) as *mut u64;
            *w &= !MOVING;
            if ((*w) as i64) < self.threshold() {
                continue;
            }
            if text { self.offer_lazy(s, p, ng)? } else { self.offer(p) }
        }
        Ok(())
    }

    unsafe fn offer_table(&mut self, t: &HashAgg) {
        let (off_used, off_count) = (t.layout.sentinel_off as usize, t.layout.slot_off[self.pl.order_slot] as usize);
        let rec = t.layout.size as usize;
        let base = t.base_ptr();
        let mut thr = self.threshold();
        for i in 0..t.cap as usize {
            let p = base.add(i * rec);
            if *(p.add(off_used) as *const u64) == 0 {
                continue;
            }
            let w = p.add(off_count) as *mut u64;
            *w &= !MOVING;
            if ((*w) as i64) < thr {
                continue;
            }
            self.offer(p);
            thr = self.threshold();
        }
    }

    /// `better(r, worst)` with `r`'s text keys rendered through the site only when the comparison
    unsafe fn better_lazy(&self, s: *mut Site, r: *const u8, w: *const u8, ng: usize, tmp: &mut [u64; MAX_KEYS], done: &mut u32) -> Result<bool, i64> {
        let (ra, rb) = (Rec::at(r as *mut u8, self.layout), Rec::at(w as *mut u8, self.layout));
        if self.pl.order == ORDER_COUNT_DESC {
            let (ca, cb) = (ra.count(self.pl.order_slot), rb.count(self.pl.order_slot));
            if ca != cb {
                return Ok(ca > cb);
            }
        }
        for k in 0..self.nkeys {
            let text = self.pl.key_kind[k] == K_TEXT && k < ng;
            let wa = if text {
                if *done >> k & 1 == 0 {
                    let raw = slot_word(&ra, self.slot_base + self.pl.rep_slot[k]);
                    if dict_value(s, (raw >> 32) as u32, self.pl.key_lane[k] as u32, raw as u32, &mut tmp[k]) < 0 {
                        return Err(ERROR);
                    }
                    *done |= 1 << k;
                }
                tmp[k]
            } else {
                self.pl.key_out(&ra, k, self.slot_base)
            };
            let wb = self.pl.key_out(&rb, k, self.slot_base);
            if wa == wb {
                continue;
            }
            let bytes = text || (self.pl.key_kind[k] == K_CODE && self.rendered);
            return Ok(if bytes { text_less(wa, wb) == core::cmp::Ordering::Less } else { (wa as i64) < (wb as i64) });
        }
        Ok(false)
    }

    /// `offer` for a text-keyed table under a key order: the kept copies carry rendered
    unsafe fn offer_lazy(&mut self, s: *mut Site, r: *const u8, ng: usize) -> Result<(), i64> {
        let mut tmp = [0u64; MAX_KEYS];
        let mut done = 0u32;
        let slot = if self.len < self.n {
            self.len += 1;
            self.len - 1
        } else {
            if !self.better_lazy(s, r, self.at(self.worst), ng, &mut tmp, &mut done)? {
                return Ok(());
            }
            self.worst
        };
        core::ptr::copy_nonoverlapping(r, self.base.add(slot * self.rec), self.rec);
        let kept = Rec::at(self.base.add(slot * self.rec), self.layout);
        for k in 0..ng {
            if self.pl.key_kind[k] != K_TEXT {
                continue;
            }
            let off = self.layout.slot_off[self.slot_base + self.pl.rep_slot[k]] as usize;
            if done >> k & 1 == 0 {
                let raw = *(kept.as_ptr().add(off) as *const u64);
                if dict_value(s, (raw >> 32) as u32, self.pl.key_lane[k] as u32, raw as u32, &mut tmp[k]) < 0 {
                    return Err(ERROR);
                }
            }
            *(kept.as_ptr().add(off) as *mut u64) = tmp[k];
        }
        if self.len == self.n {
            self.find_worst();
        }
        Ok(())
    }

    unsafe fn offer_table_lazy(&mut self, s: *mut Site, t: &HashAgg, ng: usize) -> Result<(), i64> {
        let (off_used, off_count) = (t.layout.sentinel_off as usize, t.layout.slot_off[self.pl.order_slot] as usize);
        let rec = t.layout.size as usize;
        let base = t.base_ptr();
        let mut thr = self.threshold();
        for i in 0..t.cap as usize {
            let p = base.add(i * rec);
            if *(p.add(off_used) as *const u64) == 0 {
                continue;
            }
            let w = p.add(off_count) as *mut u64;
            *w &= !MOVING;
            if ((*w) as i64) < thr {
                continue;
            }
            self.offer_lazy(s, p, ng)?;
            thr = self.threshold();
        }
        Ok(())
    }
}

/// A pair record folded into its group's accumulator record: the pair counted, the plain slots
pub unsafe fn fold_pair(pl: &Plan, acc: &Rec<'_>, pair: &Rec<'_>, ng: usize) {
    acc.add_count(0, 1);
    for j in 0..pl.nslots {
        if !pair.valid(j) {
            continue;
        }
        match pl.slot_kind[j] {
            S_SUM => acc.add_i64(j + 1, pair.i64(j)),
            S_AVG | S_AVG_TBL => {
                let (sum, n) = pair.avg(j);
                acc.add_avg(j + 1, sum, n);
            }
            S_MIN_CODE => {
                let (p, c) = pair.code(j);
                acc.min_code(j + 1, p, c);
            }
            _ => acc.add_count(j + 1, pair.count(j)),
        }
    }
    for k in 0..ng {
        if is_text(pl.key_kind[k]) {
            let (p, c) = pair.code(pl.rep_slot[k]);
            acc.min_code(1 + pl.rep_slot[k], p, c);
        }
    }
}

/// Slot `j`'s raw word (a `MinCode` representative, or the datum an owner rendered into it).
#[inline(always)]
pub unsafe fn slot_word(r: &Rec<'_>, j: usize) -> u64 {
    *(r.as_ptr().add(r.layout.slot_off[j] as usize) as *const u64)
}

pub fn slot_word_of(r: &Rec<'_>, j: usize) -> u64 {
    // SAFETY: a `Rec` is a live record of its layout; the slot offset is inside it.
    unsafe { slot_word(r, j) }
}
