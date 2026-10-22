//! keyed.twopass's top-rule export owner (ops/sort/topn/twopass_top.rs, V4R-192): its owner phase, the spec read back
//! from the owner words and the singles' heap, moved here unchanged (rule R6: the family's unsafe code lives in raw.rs).
//! twopass_top.rs keeps the spec, its word layout and `owner_fns`, which hands these out.
use gravity_abi::breaker::{HashAgg, OwnerTop, RecordLayout, HASHAGG_HDR, MOVING, OWNERTOP_HDR, OWNER_PARAMS, OWNER_RUN_OFF};
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH};
use gravity_abi::Site;

use crate::ops::sort::topn::twopass_top::{TopExportSpec, LAYOUT_WORDS, P_HEAD, P_LAYOUT, P_N};

unsafe fn spec_of(params: *const u64) -> TopExportSpec {
    let w = core::slice::from_raw_parts(params, OWNER_PARAMS);
    let mut words = [0u64; LAYOUT_WORDS];
    words.copy_from_slice(&w[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS]);
    // SAFETY: the words `owner_fns` packed.
    let layout: RecordLayout = RecordLayout::from_words(words);
    let (h, f) = (w[P_HEAD], w[P_HEAD] >> 16);
    TopExportSpec { layout, count_slot: h as u8, img_words: (h >> 8) as u8, n: w[P_N] as u32, desc: f & 1 != 0, nulls_first: f & 2 != 0, nullable: f & 4 != 0, ext: f & 8 != 0, ext_nn: f & 16 != 0, spread: f & 32 != 0 }
}

/// The singles' heap bytes, then the kept run's first byte.
fn runs_off(sp: &TopExportSpec) -> usize {
    OWNER_RUN_OFF + sp.n as usize * sp.layout.size() as usize
}

pub(in crate::ops::sort::topn) fn owner_min(params: *const u64, _owners: u32) -> u64 {
    // SAFETY: `OWNER_PARAMS` words.
    let sp = unsafe { spec_of(params) };
    (runs_off(&sp) + 2 * (HASHAGG_HDR + 64 * sp.layout.size() as usize)) as u64
}

pub(in crate::ops::sort::topn) unsafe fn owner_open(params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    let sp = spec_of(params);
    let (rec, base) = (sp.layout.size() as usize, runs_off(&sp));
    let off = base + ((bytes.saturating_sub(base) / 2) & !7);
    if rec % 8 != 0 || off + HASHAGG_HDR + 2 * rec > bytes {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: 0, table_off: off as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    off as i64
}

/// A key seen once's place in the top's tie order (smaller = kept first): twopass.rs `Single::of` over `key_of`'s words.
/// Under an extreme top, the key's extreme's place on the top's order (lowerv4 raw.rs `ext_rank` of its witness).
unsafe fn rank(sp: &TopExportSpec, r: *const u8) -> (u8, i64) {
    if sp.ext {
        return ext_ranks(sp, r).0;
    }
    let w = r as *const u64;
    let (w0, w1) = (*w, if sp.img_words >= 2 { *w.add(1) } else { 0 });
    match sp.nullable && w1 != 0 {
        true => (if sp.nulls_first { 0 } else { 2 }, 0),
        false => (1, if sp.desc { !(w0 as i64) } else { w0 as i64 }),
    }
}

/// An extreme top's key `r`: its witness's place (the extreme, or NULL when no row has a value) and its best possible
/// group's (NULL under NULLS FIRST when a row's value is NULL), as lowerv4 raw.rs `ext_admitted` reads them.
unsafe fn ext_ranks(sp: &TopExportSpec, r: *const u8) -> ((u8, i64), (u8, i64)) {
    let word = |j: usize| *(r.add(sp.layout.slot_off(j) as usize) as *const i64);
    let cs = sp.count_slot as usize;
    let (c, e) = (word(cs), word(cs + 1));
    let nn = if sp.ext_nn { word(cs + 2) } else { c };
    let at = |v: Option<i64>| match v {
        None => (if sp.nulls_first { 0 } else { 2 }, 0),
        Some(x) => (1, if sp.desc { !x } else { x }),
    };
    let witness = (nn > 0).then_some(e);
    // gtopn GT-1: a spread key's best is its column's other extreme, the word after the witness (and count(v))
    let top = if sp.spread { (nn > 0).then(|| word(cs + 2 + sp.ext_nn as usize)) } else { witness };
    (at(witness), at(if sp.nulls_first && nn < c { None } else { top }))
}

/// The owner's `n` best singles: a heap in the region with the worst at the root.
struct Singles<'a> {
    sp: &'a TopExportSpec,
    base: *mut u8,
    rec: usize,
    len: usize,
}

impl Singles<'_> {
    unsafe fn at(&self, i: usize) -> *mut u8 {
        self.base.add(i * self.rec)
    }

    unsafe fn worse(&self, a: usize, b: usize) -> bool {
        rank(self.sp, self.at(a)) > rank(self.sp, self.at(b))
    }

    unsafe fn swap(&self, a: usize, b: usize) {
        core::ptr::swap_nonoverlapping(self.at(a), self.at(b), self.rec);
    }

    unsafe fn offer(&mut self, r: *const u8) {
        let n = self.sp.n as usize;
        if self.len < n {
            let mut i = self.len;
            core::ptr::copy_nonoverlapping(r, self.at(i), self.rec);
            self.len += 1;
            while i > 0 && self.worse(i, (i - 1) / 2) {
                self.swap(i, (i - 1) / 2);
                i = (i - 1) / 2;
            }
            return;
        }
        if n == 0 || rank(self.sp, r) >= rank(self.sp, self.at(0)) {
            return;
        }
        core::ptr::copy_nonoverlapping(r, self.at(0), self.rec);
        let mut i = 0;
        loop {
            let (l, rr) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.len && self.worse(l, m) {
                m = l;
            }
            if rr < self.len && self.worse(rr, m) {
                m = rr;
            }
            if m == i {
                return;
            }
            self.swap(i, m);
            i = m;
        }
    }
}

/// The kept run's next record: `r` copied with its count word's `MOVING` mark cleared (`admitted` reads the word raw).
unsafe fn keep(sp: &TopExportSpec, h: &mut OwnerTop, owner: *mut u8, r: *const u8) -> bool {
    let rec = sp.layout.size() as usize;
    let off = runs_off(sp) + h.total as usize;
    if off + rec > h.table_off as usize {
        return false;
    }
    let d = owner.add(off);
    core::ptr::copy_nonoverlapping(r, d, rec);
    let c = d.add(sp.layout.slot_off(sp.count_slot as usize) as usize) as *mut u64;
    *c &= !MOVING;
    h.total += rec as u64;
    h.len += 1;
    let dir = owner.add(OWNERTOP_HDR) as *mut u32;
    (*dir, *dir.add(1), h.worst) = (runs_off(sp) as u32, h.len, 1);
    true
}

pub(in crate::ops::sort::topn) unsafe fn owner_reduce(params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut Site, _cand: *const u32, _ncand: i64) -> i64 {
    let sp = spec_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let t = &mut *(table as *mut HashAgg);
    h.records += t.len as u64;
    let (g, cs, rec) = (sp.layout.partition_keys(), sp.count_slot as usize, sp.layout.size() as usize);
    let mut singles = Singles { sp: &sp, base: owner.add(OWNER_RUN_OFF), rec, len: h.resume as usize };
    if sp.ext {
        // one record per key (never marked): its extreme offered to the heap, the record kept while its best ranks at the
        // owner's n-th best extreme so far or before (the bound only tightens, so a key left out stays out; every heap
        // member's best is at most its own extreme, so it is kept)
        for i in 0..t.cap as usize {
            let r = t.record(i);
            if !r.used() {
                continue;
            }
            h.groups += 1;
            singles.offer(r.as_ptr());
            let full = sp.n > 0 && singles.len == sp.n as usize;
            if !full || ext_ranks(&sp, r.as_ptr()).1 <= rank(&sp, singles.at(0)) {
                if !keep(&sp, h, owner, r.as_ptr()) {
                    return unsupported(UNSUPPORTED_SCRATCH);
                }
            }
        }
        h.resume = singles.len as u32;
        return 0;
    }
    if g == sp.layout.nkeys() {
        // one record per key: the table as it stands
        for i in 0..t.cap as usize {
            let r = t.record(i);
            if !r.used() {
                continue;
            }
            h.groups += 1;
            match r.count(cs) {
                c if c >= 2 => {
                    if !keep(&sp, h, owner, r.as_ptr()) {
                        return unsupported(UNSUPPORTED_SCRATCH);
                    }
                }
                1 => singles.offer(r.as_ptr()),
                _ => {}
            }
        }
    } else {
        // a marked record: the key's part records side by side, summed
        t.sort_dense_by_words(g);
        let words = |p: *const u8| core::slice::from_raw_parts(p as *const u64, g);
        let n = t.len as usize;
        let mut i = 0;
        while i < n {
            let first = t.record(i);
            let (mut c, mut j) = (first.count(cs), i + 1);
            while j < n && words(t.record(j).as_ptr()) == words(first.as_ptr()) {
                c += t.record(j).count(cs);
                j += 1;
            }
            h.groups += 1;
            if c >= 2 {
                for k in i..j {
                    if !keep(&sp, h, owner, t.record(k).as_ptr()) {
                        return unsupported(UNSUPPORTED_SCRATCH);
                    }
                }
            } else if c == 1 {
                singles.offer(first.as_ptr());
            }
            i = j;
        }
    }
    h.resume = singles.len as u32;
    0
}

/// The round's end: the singles' heap is run 1 (after the kept run, empty or not).
pub(in crate::ops::sort::topn) unsafe fn owner_end(params: *const u64, owner: *mut u8) -> i64 {
    let sp = spec_of(params);
    let h = &mut *(owner as *mut OwnerTop);
    let dir = owner.add(OWNERTOP_HDR) as *mut u32;
    if h.worst == 0 {
        (*dir, *dir.add(1)) = (runs_off(&sp) as u32, 0);
    }
    let singles = if sp.ext { 0 } else { h.resume };
    (*dir.add(2), *dir.add(3), h.worst) = (OWNER_RUN_OFF as u32, singles, 2);
    h.len += singles;
    h.resume = 0;
    0
}
