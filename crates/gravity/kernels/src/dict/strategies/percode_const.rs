//! The constant-key window's fold (decision 14 (B), V4R-19; lowerv4 keyed/percode_const.rs): a window whose one group key
//! the source proved constant is folded before its rows, in one pass over the window's code lane: the rows counted and one
//! per-code length summed, read from the length table the window's bind returned (the dictionary index in place, R4: a
//! part property, nothing built per statement). The pass is the histogram's dot product `sum(n[c] x len[c])` taken row
//! by row: one key per window leaves one sum, so no per-code count is kept. A filter that is one per-code verdict of the
//! column (B3-05: a comparison with any constant, a LIKE / ILIKE / regex body of it alone, its bind's bits) is a survivor
//! mask on the same pass, the code's bit read per row. A condition-cache miss is folded too when the verdicts are every
//! covered qual (`PK_COVERED`), its survivor words set by the same pass (CACHE-MISS lever a). Budget 150 lines
//! (files-extra.tsv, +25 % gate). Written by M1-STEP; owner M1-HOSTS.
use gravity_abi::PtrWord;
use core::sync::atomic::{AtomicU64, Ordering};

use gravity_abi::ctx::{answer, lane, site_ptr, Lane};
use gravity_abi::Ctx;

use crate::host_abi::{BIND_FORM_IDX_BYTES, BIND_FORM_IDX_CHARS, BIND_FORM_IDX_DELTA, BIND_FORM_WORDS, BIND_OUT_WORDS};

/// `flags`: the fold reads the code lane (a `count(col)` or a length), sums the length table's value, and drops the rows
/// whose code's bit is clear in any of the pipeline's per-code verdicts of the column (B3-05, BG-03: `vs`, a `Verdicts` list relative to
/// `tab`'s out words, one out array; `tab` is the first verdict's own when there is no length), and keeps the kept rows'
/// least and greatest code (`PK_MINMAX`).
pub const PK_CODES: u64 = 1;
pub const PK_LEN: u64 = 2;
/// CACHE-MISS lever a: every filter op of the pipeline is one the condition cache covers, so the verdicts are the covered
/// quals and a row passes them iff its code is kept: a miss window is folded and each kept row's bit set in its survivor
/// words, granule-relative (the selection's entry, as frame/site.rs `row_index`): the cache fills as the rows would.
pub const PK_COVERED: u64 = 4;
pub const PK_VERDICT: u64 = 8;
/// KEYED-15: the fold keeps the least and the greatest kept code too (`answer.min` / `answer.max`, `ANSWER_MINMAX`): a
/// part's dictionary is byte-rank sorted, so they are the kept rows' least and greatest values under a collation that
/// orders by bytes.
pub const PK_MINMAX: u64 = 16;
/// BG-03 / B3-05: the summed table is a per-code word table of any integer function of the column (`tab` a `Words`
/// table: data, faults, form `BIND_FORM_WORDS`), its words the result's datum of `flags >> PK_SIGNED_SHIFT & 0xf` bytes
/// (2, 4 or 8), sign-extended and summed exactly (128 bits); a kept row whose code's entry is faulted (a raise, a NULL
/// result, an entry the rows fill) declines the window.
pub const PK_SIGNED: u64 = 64;
pub const PK_SIGNED_SHIFT: u32 = 8;
/// D3154: the fold keeps the kept rows' least and greatest value of the read table too (a length, or a `PK_SIGNED`
/// word sign-extended), as `answer.min` / `answer.max` words with no `ANSWER_MINMAX` flag (values, not codes): a
/// `min` / `max` of the length. Never beside `PK_MINMAX` (one record's fields).
pub const PK_LENMM: u64 = 128;
/// `fold`'s form for `PK_SIGNED` (not a bind form).
const SIGNED_WORDS: u64 = u64::MAX - 1;
/// `fold`'s verdicts: none read, read with no fault words (every one the zero page), read with their fault words.
const NO_V: u8 = 0;
const CLEAN_V: u8 = 1;
const FAULT_V: u8 = 2;

/// B3-05 / B3-06 / BG-02 / BG-03 (mechanism D, one code verdict): the pipeline's per-code verdicts of one code column,
/// ANDed per code. `vs` = `[n, d_1 .. d_n]`: verdict `i`'s bind out words `(bits, faults)` sit `d_i` (signed) tables from
/// `base`'s, all in the window's one bind out array. Any number of them; one is read without the list's loop.
#[derive(Clone, Copy)]
pub struct Verdicts {
    base: *const u64,
    vs: *const i64,
    n: usize,
    one: (*const u64, *const u64),
    /// Every verdict's fault words are the zero page (codes.rs `zero_faults`): no entry is faulted.
    clean: bool,
}

impl Verdicts {
    /// The list at `vs` over out array `base`: `Some(None)` when there is none (`vs` null or `n` 0), `None` when a verdict's
    /// bits or fault words are unbound (the window takes its rows).
    ///
    /// # Safety
    /// `vs` is null or `[n, d_1 .. d_n]`; `base` and every `base + d_i` tables are bound out words of one array.
    pub unsafe fn of(base: *const u64, vs: *const i64) -> Option<Option<Self>> {
        let n = if vs.is_null() { 0 } else { *vs as usize };
        if n == 0 {
            return Some(None);
        }
        if base.is_null() {
            return None;
        }
        let v = Verdicts { base, vs, n, one: (core::ptr::null(), core::ptr::null()), clean: true };
        let mut clean = true;
        for i in 0..n {
            let t = v.out(i);
            if *t == 0 || *t.add(1) == 0 {
                return None;
            }
            clean &= *t.add(1) as *const u64 == crate::dict::codes::zero_faults();
        }
        let t = v.out(0);
        Some(Some(Verdicts { one: (*t as *const u64, *t.add(1) as *const u64), clean, ..v }))
    }

    #[inline(always)]
    unsafe fn out(&self, i: usize) -> *const u64 {
        self.base.offset(*self.vs.add(1 + i) as isize * BIND_OUT_WORDS as isize)
    }

    #[inline(always)]
    unsafe fn bit(words: *const u64, c: u32) -> bool {
        (*words.add((c >> 6) as usize) >> (c & 63)) & 1 != 0
    }

    /// Code `c`'s entry is faulted in any verdict (a raise, or a D2074 table the rows fill): its rows would raise or fill it.
    ///
    /// # Safety
    /// `c` is a code of the bound part (below every table's entry count).
    #[inline(always)]
    pub unsafe fn faulted(&self, c: u32) -> bool {
        if self.n == 1 {
            return Self::bit(self.one.1, c);
        }
        (0..self.n).any(|i| Self::bit(*self.out(i).add(1) as *const u64, c))
    }

    /// Every verdict passes code `c`.
    ///
    /// # Safety
    /// As `faulted`'s.
    #[inline(always)]
    pub unsafe fn keeps(&self, c: u32) -> bool {
        if self.n == 1 {
            return Self::bit(self.one.0, c);
        }
        (0..self.n).all(|i| Self::bit(*self.out(i) as *const u64, c))
    }
}

/// `agg_percode_const`'s windows so far in this process (the census line's `pkconst=`): folded (a hit, or no condition
/// cache on the site), folded on a condition-cache miss (its survivor words filled here), declined on a miss (a fold
/// without `PK_COVERED`: some covered qual is not this pass's), declined otherwise (a lane that may hold a NULL, not a
/// decoded 4-byte code lane, an unknown length form, a verdict with a faulted entry).
static CENSUS: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
const C_FOLDED: usize = 0;
const C_FILLED: usize = 1;
const C_MISS: usize = 2;
const C_OTHER: usize = 3;

/// `[folded, folded on a miss, declined on a miss, declined otherwise]` (CENSUS).
pub fn percode_const_census() -> [u64; 4] {
    core::array::from_fn(|i| CENSUS[i].load(Ordering::Relaxed))
}

/// The survivor words of site `s`'s window when it is a condition-cache miss: its verdict lane (`LaneId::Verdict(0)`, the
/// cache's) not covered, whose words the rows passing every covered step set (lowerv4 source/verdict_cond.rs `record`) and
/// the granule host publishes once the window is consumed; null for a hit or a site without the cache.
pub unsafe fn miss_words(s: *const gravity_abi::site::Site) -> *mut u64 {
    let (lanes, verdict) = ((*s).lanes.ptr() as *const Lane, lane::id(lane::ID_VERDICT, 0));
    let miss = (0..(*s).nlanes as usize).map(|l| &*lanes.add(l)).find(|l| l.id == verdict && !l.ptr.ptr().is_null() && l.flags & lane::VERDICT_MASK != lane::VERDICT_ALL_MATCH);
    miss.map_or(core::ptr::null_mut(), |l| l.ptr.ptr() as *mut u64)
}


/// D3179: whether site `s`'s window is a condition-cache hit: its verdict lane carries the cached words, `ALL_MATCH` (the
/// frame's hit, lowerv4 verdict_cond.rs `window`). A hit's window decodes its lanes whole (the host's sparse stage never
/// runs on one).
unsafe fn hit(s: *const gravity_abi::site::Site) -> bool {
    let (lanes, verdict) = ((*s).lanes.ptr() as *const Lane, lane::id(lane::ID_VERDICT, 0));
    (0..(*s).nlanes as usize).map(|l| &*lanes.add(l)).any(|l| l.id == verdict && !l.ptr.ptr().is_null() && l.flags & lane::VERDICT_MASK == lane::VERDICT_ALL_MATCH)
}

crate::contract::host! {
    /// Window `[cursor, end)` of site `at >> 8` (its selection's entries when it has one) over code lane `at & 0xff`, into the
    /// answer record `out`: `nonnull` = the rows kept, the exact sum of the length (`tab`: the bound table's words `(data, faults,
    /// form)`), under `PK_MINMAX` `min` / `max` = the least / greatest kept code (`ANSWER_MINMAX`; no kept row: u32::MAX / 0).
    /// 0: folded; 1: declined, `out` unwritten (the window takes its rows): a code lane that is not a decoded 4-byte
    /// code lane or may hold a NULL, a length form this pass does not read, a verdict with a faulted entry, a condition-cache
    /// miss without `PK_COVERED` (a covered qual may be none of the verdicts). A miss under `PK_COVERED` is folded and
    /// fills its survivor words; a fault met part way declines with some words set, each a survivor's, which the rows
    /// set again (the rows OR their bits in).
    pub unsafe fn agg_percode_const(ctx: *mut Ctx, at: u64, cursor: u64, end: u64, tab: *const u64, flags: u64, vs: *const i64, out: *mut u8) -> i64 {
        // the extremes' passes are their own functions: the counts' and lengths' passes keep their code (and its layout) as before
        match flags & (PK_MINMAX | PK_LENMM) {
            0 => window::<false, false>(ctx, at, cursor, end, tab, flags, vs, out),
            PK_MINMAX => window_minmax(ctx, at, cursor, end, tab, flags, vs, out),
            PK_LENMM if flags & (PK_LEN | PK_SIGNED) != 0 => window_lenmm(ctx, at, cursor, end, tab, flags, vs, out),
            _ => {
                CENSUS[C_OTHER].fetch_add(1, Ordering::Relaxed);
                1
            }
        }
    }
}

/// `agg_percode_const` under `PK_MINMAX` (not inlined into the host: `window::<false>` stays the counts' pass alone).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn window_minmax(ctx: *mut Ctx, at: u64, cursor: u64, end: u64, tab: *const u64, flags: u64, vs: *const i64, out: *mut u8) -> i64 {
    window::<true, false>(ctx, at, cursor, end, tab, flags, vs, out)
}

/// D3154: `agg_percode_const` under `PK_LENMM` (its own function, as `window_minmax`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn window_lenmm(ctx: *mut Ctx, at: u64, cursor: u64, end: u64, tab: *const u64, flags: u64, vs: *const i64, out: *mut u8) -> i64 {
    window::<false, true>(ctx, at, cursor, end, tab, flags, vs, out)
}

/// `agg_percode_const`'s pass over the window (its doc); `MM`: `PK_MINMAX`, the least / greatest kept code too; `LM`:
/// `PK_LENMM`, the least / greatest kept value of the table.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn window<const MM: bool, const LM: bool>(ctx: *mut Ctx, at: u64, cursor: u64, end: u64, tab: *const u64, flags: u64, vs: *const i64, out: *mut u8) -> i64 {
    let s = site_ptr(ctx, (at >> 8) as usize);
    let lanes = (*s).lanes.ptr() as *const Lane;
    let words = miss_words(s);
    if !words.is_null() && flags & PK_COVERED == 0 {
        CENSUS[C_MISS].fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    // the selection is read as it is: a selective verdict's few entries, not the window's every row; with no condition
    // cache it can be the sparse stage's (code.sparse), which writes the codes of the rows it selects alone, so the other
    // rows hold a previous window's codes (D3179)
    let (sel, from, to) = ((*s).sel.ptr(), cursor as usize, end as usize);
    let (n, sum, hi, mm, lm) = match flags & PK_CODES {
        0 if LM => {
            CENSUS[C_OTHER].fetch_add(1, Ordering::Relaxed);
            return 1;
        }
        0 => (to.saturating_sub(from) as u64, 0, 0, (u32::MAX, 0), (i64::MAX, i64::MIN)),
        _ => {
            let c = &*lanes.add((at & 0xff) as usize);
            if c.kind != lane::KIND_CODES || c.width != 4 || c.ptr.ptr().is_null() || (!c.valid.ptr().is_null() && c.flags & lane::FLAG_ALL_VALID == 0) {
                CENSUS[C_OTHER].fetch_add(1, Ordering::Relaxed);
                return 1;
            }
            let (codes, idx) = (c.ptr.ptr() as *const u32, if flags & (PK_LEN | PK_SIGNED) != 0 { *tab as *const u8 } else { core::ptr::null() });
            let form = match flags & (PK_LEN | PK_SIGNED) {
                0 => u64::MAX,
                PK_LEN => *tab.add(2),
                // the signed word form, its fault words and its datum width read as a shift
                _ if *tab.add(2) == BIND_FORM_WORDS && matches!(flags >> PK_SIGNED_SHIFT & 0xf, 2 | 4 | 8) => SIGNED_WORDS,
                _ => u64::MAX - 2,
            };
            let lf = (form == SIGNED_WORDS).then(|| (*tab.add(1) as *const u64, 64 - 8 * (flags >> PK_SIGNED_SHIFT & 0xf) as u32));
            // the verdict's bits (`code >> 6` the word, `code & 63` the bit) and its fault words (a zero page when it has
            // none: codes.rs `agg_bind`; every entry of a table the rows fill, D2074, until a row filled it): a row whose
            // code is faulted would raise or fill it, so the window takes its rows
            let vs = if flags & PK_VERDICT != 0 { vs } else { core::ptr::null() };
            let Some(v) = Verdicts::of(tab, vs) else {
                CENSUS[C_OTHER].fetch_add(1, Ordering::Relaxed);
                return 1;
            };
            // D3179: under PK_COVERED a hit's selection (or, with none, the window's every row) is the rows that pass every
            // filter, so the verdicts are not read again
            let v = if words.is_null() && flags & PK_COVERED != 0 && hit(s) { None } else { v };
            // the verdicts' fault words are read only when some verdict has any
            let vk = match v {
                None => NO_V,
                Some(v) if v.clean => CLEAN_V,
                Some(_) => FAULT_V,
            };
            macro_rules! go {
                ($f:ident) => {
                    match (sel.is_null(), words.is_null(), vk) {
                        (true, true, NO_V) => fold::<true, $f, false, NO_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (false, true, NO_V) => fold::<false, $f, false, NO_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (true, true, CLEAN_V) => fold::<true, $f, false, CLEAN_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (false, true, CLEAN_V) => fold::<false, $f, false, CLEAN_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (true, true, _) => fold::<true, $f, false, FAULT_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (false, true, _) => fold::<false, $f, false, FAULT_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (true, false, CLEAN_V) => fold::<true, $f, true, CLEAN_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (false, false, CLEAN_V) => fold::<false, $f, true, CLEAN_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (true, false, _) => fold::<true, $f, true, FAULT_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                        (false, false, _) => fold::<false, $f, true, FAULT_V, MM, LM>(codes, sel, from, to, idx, lf, words, v),
                    }
                };
            }
            const NO_LEN: u64 = u64::MAX;
            let folded = match form {
                NO_LEN => go!(NO_LEN),
                SIGNED_WORDS => go!(SIGNED_WORDS),
                BIND_FORM_WORDS => go!(BIND_FORM_WORDS),
                BIND_FORM_IDX_BYTES => go!(BIND_FORM_IDX_BYTES),
                BIND_FORM_IDX_DELTA => go!(BIND_FORM_IDX_DELTA),
                BIND_FORM_IDX_CHARS => go!(BIND_FORM_IDX_CHARS),
                _ => None,
            };
            match folded {
                Some(r) => r,
                None => {
                    CENSUS[C_OTHER].fetch_add(1, Ordering::Relaxed);
                    return 1;
                }
            }
        }
    };
    CENSUS[if words.is_null() { C_FOLDED } else { C_FILLED }].fetch_add(1, Ordering::Relaxed);
    let o = out as *mut u64;
    o.add(answer::NONNULL / 8).write(n);
    o.add(answer::SUM_LO / 8).write(sum);
    o.add(answer::SUM_HI / 8).write(hi);
    if MM {
        o.add(answer::MIN / 8).write(u64::from(mm.0));
        o.add(answer::MAX / 8).write(u64::from(mm.1));
        o.add(answer::FLAGS / 8).write(gravity_abi::ctx::ANSWER_MINMAX);
    }
    if LM {
        // D3154: the kept values' extremes (the identities when no row is kept: `nonnull` 0 folds neither)
        o.add(answer::MIN / 8).write(lm.0 as u64);
        o.add(answer::MAX / 8).write(lm.1 as u64);
        o.add(answer::FLAGS / 8).write(0);
    }
    0
}

/// The kept rows, their lengths' sum and (`MM`) their least and greatest code (else `(u32::MAX, 0)`): row `j`'s index is
/// itself (`PLAIN`) or the selection's entry `j`; under `V` (not `NO_V`) a row whose code any verdict of `v` clears is
/// dropped; `None` under `FAULT_V` at a row whose code a verdict faults (nothing written to the answer). Form `F`'s read is lowerv4
/// percode_len.rs `len_read`'s at the index's entry (`u64::MAX`: none), each read within its entry (8 of a word table's 8
/// bytes, of an index's 12). `FILL`: each kept row's bit is set in `words` (a plain window's gathered per word in a
/// register, a selection's set in place).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
unsafe fn fold<const PLAIN: bool, const F: u64, const FILL: bool, const V: u8, const MM: bool, const LM: bool>(codes: *const u32, sel: *const u32, from: usize, to: usize, idx: *const u8, lf: Option<(*const u64, u32)>, words: *mut u64, v: Option<Verdicts>) -> Option<(u64, u64, u64, (u32, u32), (i64, i64))> {
    let (mut n, mut sum, mut at, mut mm, mut lm) = (0u64, 0u64, from, (u32::MAX, 0u32), (i64::MAX, i64::MIN));
    // SIGNED_WORDS: the exact signed sum, and the table's fault words with the datum's shift
    let (mut ssum, (lfaults, shift)) = (0i128, if F == SIGNED_WORDS { lf? } else { (core::ptr::null(), 0) });
    let v = if V != NO_V { v? } else { Verdicts { base: core::ptr::null(), vs: core::ptr::null(), n: 0, one: (core::ptr::null(), core::ptr::null()), clean: true } };
    while at < to {
        let stop = if PLAIN && FILL { to.min((at | 63) + 1) } else { to };
        let mut m = 0u64;
        for j in at..stop {
            let i = if PLAIN { j } else { *sel.add(j) as usize };
            let c = *codes.add(i);
            if V == FAULT_V && v.faulted(c) {
                return None;
            }
            let keep = (V == NO_V || v.keeps(c)) as u64;
            if F == SIGNED_WORDS && keep != 0 && (*lfaults.add((c >> 6) as usize) >> (c & 63)) & 1 != 0 {
                return None;
            }
            let len = match F {
                BIND_FORM_WORDS => (idx.add(8 * c as usize) as *const u64).read_unaligned(),
                SIGNED_WORDS => {
                    let w = (idx.add(8 * c as usize) as *const u64).read_unaligned();
                    let x = ((w << shift) as i64) >> shift;
                    ssum += i128::from(x) & -i128::from(keep);
                    if LM && keep != 0 {
                        lm = (lm.0.min(x), lm.1.max(x));
                    }
                    0
                }
                BIND_FORM_IDX_BYTES | BIND_FORM_IDX_DELTA | BIND_FORM_IDX_CHARS => {
                    let v = (idx.add(12 * c as usize) as *const u64).read_unaligned();
                    let (lo, hi) = (v & 0xffff_ffff, v >> 32);
                    match F {
                        BIND_FORM_IDX_BYTES => lo,
                        BIND_FORM_IDX_DELTA => lo.wrapping_sub(hi) & 0xffff_ffff,
                        _ => hi,
                    }
                }
                _ => 0,
            };
            n += keep;
            sum = sum.wrapping_add(len & keep.wrapping_neg());
            if LM && F != SIGNED_WORDS && keep != 0 {
                // a length (an index entry's 32 bits, a word table's word): never past i64
                lm = (lm.0.min(len as i64), lm.1.max(len as i64));
            }
            if MM {
                // a dropped row leaves both: the identities' own codes
                let (lo, hi) = if keep != 0 { (c, c) } else { (u32::MAX, 0) };
                mm = (mm.0.min(lo), mm.1.max(hi));
            }
            if FILL && PLAIN {
                m |= keep << (i & 63);
            } else if FILL {
                *words.add(i >> 6) |= keep << (i & 63);
            }
        }
        if FILL && PLAIN {
            *words.add(at >> 6) |= m;
        }
        at = stop;
    }
    if F == SIGNED_WORDS {
        return Some((n, ssum as u64, (ssum >> 64) as u64, mm, lm));
    }
    Some((n, sum, 0, mm, lm))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-site ctx whose lanes are `lanes` (the site's words, then the ctx's, kept alive by the caller).
    fn ctx_of(lanes: &mut [Lane], sel: &[u32]) -> Vec<u64> {
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Cold
        let c = ctx.as_mut_ptr() as *mut Ctx;
        // SAFETY: the words are a zeroed ctx of one site (every field an integer, a pointer or an Option of one).
        unsafe {
            (*c).sites = (c as *mut u8).add(gravity_abi::ctx::site_off(0)) as *mut gravity_abi::site::Site;
            let s = site_ptr(c, 0);
            ((*s).lanes, (*s).nlanes) = (lanes.as_mut_ptr(), lanes.len() as u16);
            (*s).sel = if sel.is_empty() { core::ptr::null() } else { sel.as_ptr() };
            ((*s).sel_n, (*s).rows) = (sel.len() as u32, 1000);
        }
        ctx
    }

    fn code_lane(codes: &[u32]) -> Lane {
        Lane { ptr: codes.as_ptr() as *const u8, valid: core::ptr::null(), width: 4, kind: lane::KIND_CODES, region: 0, flags: 0, ncodes_or_scale: 0, id: 0, aux: 0 }
    }

    /// `(rows kept, lengths' sum)` of the window, or `None` declined.
    fn run(lanes: &mut [Lane], sel: &[u32], win: (u64, u64), tab: &[u64; 3], flags: u64) -> Option<(u64, u64)> {
        run_at(lanes, sel, win, tab.as_ptr(), flags, &[])
    }

    /// `run` with `tab` the out words of a table in a bind's out array (B3-05: the verdict's sit beside it).
    /// `ds`: the verdicts' tables relative to `tab` (`Verdicts`), read under `PK_VERDICT`.
    fn run_at(lanes: &mut [Lane], sel: &[u32], win: (u64, u64), tab: *const u64, flags: u64, ds: &[i64]) -> Option<(u64, u64)> {
        let mut ctx = ctx_of(lanes, sel);
        let mut out = [0u64; answer::SIZE / 8];
        let vs: Vec<i64> = std::iter::once(ds.len() as i64).chain(ds.iter().copied()).collect(); // alloc: Cold
        // SAFETY: `ctx` is a one-site ctx over `lanes`, `tab` a bound table's words, `out` an answer record.
        let r = unsafe { agg_percode_const(ctx.as_mut_ptr() as *mut Ctx, 0, win.0, win.1, tab, flags, vs.as_ptr(), out.as_mut_ptr() as *mut u8) };
        (r == 0).then(|| (out[answer::NONNULL / 8], out[answer::SUM_LO / 8]))
    }

    /// `run` under `PK_MINMAX`: `(rows kept, least kept code, greatest kept code)`, or `None` declined.
    fn run_mm(lanes: &mut [Lane], sel: &[u32], win: (u64, u64), tab: &[u64; 3], flags: u64) -> Option<(u64, u64, u64)> {
        let mut ctx = ctx_of(lanes, sel);
        let mut out = [0u64; answer::SIZE / 8];
        let vs = [1i64, 0];
        // SAFETY: as `run_at`'s
        let r = unsafe { agg_percode_const(ctx.as_mut_ptr() as *mut Ctx, 0, win.0, win.1, tab.as_ptr(), flags | PK_MINMAX, vs.as_ptr(), out.as_mut_ptr() as *mut u8) };
        (r == 0).then(|| (out[answer::NONNULL / 8], out[answer::MIN / 8], out[answer::MAX / 8]))
    }

    /// KEYED-15: under `PK_MINMAX` the fold answers the kept rows' least and greatest code beside the count, plain and over a
    /// selection, a verdict's clear codes dropped; no kept row answers the identities.
    #[test]
    fn a_window_keeps_its_least_and_greatest_code() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let sel: Vec<u32> = (0..1000u32).filter(|i| i % 3 != 1).collect(); // alloc: Cold
        let want = |rows: &mut dyn Iterator<Item = usize>, keep: &dyn Fn(u32) -> bool| {
            rows.map(|i| codes[i]).filter(|&c| keep(c)).fold((0u64, u32::MAX as u64, 0u64), |(n, lo, hi), c| (n + 1, lo.min(c as u64), hi.max(c as u64)))
        };
        let tab = [0u64; 3];
        assert_eq!(run_mm(&mut [code_lane(&codes)], &[], (100, 900), &tab, PK_CODES), Some(want(&mut (100..900), &|_| true)), "plain");
        assert_eq!(run_mm(&mut [code_lane(&codes)], &sel, (10, 600), &tab, PK_CODES), Some(want(&mut sel[10..600].iter().map(|&i| i as usize), &|_| true)), "selected");
        // a verdict passing codes 40 .. 59 only (its table the out array's first)
        let mut bits = [0u64; 2];
        (40..60u32).for_each(|c| bits[(c >> 6) as usize] |= 1 << (c & 63));
        let zero = [0u64; 2];
        let v = [bits.as_ptr() as u64, zero.as_ptr() as u64, 0];
        let fl = PK_CODES | PK_VERDICT;
        assert_eq!(run_mm(&mut [code_lane(&codes)], &[], (0, 1000), &v, fl), Some(want(&mut (0..1000), &|c| (40..60).contains(&c))), "a verdict");
        assert_eq!(codes[0], 0);
        assert_eq!(run_mm(&mut [code_lane(&codes)], &[], (0, 1), &v, fl), Some((0, u32::MAX as u64, 0)), "no kept row (its one row's code the verdict clears): the identities");
    }

    /// B3-05: a verdict's clear bit drops its code's rows, beside or without a length, its table after or before the
    /// length's in the out array, over a selection, the whole one read entry by entry too (a selective verdict's few rows).
    /// A verdict with no bits, a row whose code's entry is faulted (not one no row reads), and a condition-cache miss under
    /// a verdict without `PK_COVERED`, decline; a covered miss folds and sets its kept rows' bits, plain and selected; a
    /// covered hit reads its selection alone.
    #[test]
    fn a_verdict_masks_its_codes() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let pass = |c: u32| c % 3 == 0 || c == 5;
        let (mut bits, zero, mut faults) = ([0u64; 2], [0u64; 2], [0u64; 2]);
        (0..97u32).filter(|&c| pass(c)).for_each(|c| bits[(c >> 6) as usize] |= 1 << (c & 63));
        faults[1] = 1 << 3;
        let lens: Vec<u64> = (0..97u64).map(|c| c % 13 + 1).collect(); // alloc: Cold
        let lane = || Lane { ncodes_or_scale: 97, ..code_lane(&codes) };
        let want = |rows: &mut dyn Iterator<Item = usize>| rows.map(|i| codes[i]).filter(|&c| pass(c)).fold((0, 0), |(n, s), c| (n + 1, s + lens[c as usize]));
        let (l, v) = ([lens.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS], [bits.as_ptr() as u64, zero.as_ptr() as u64, 0]);
        let (after, before) = ([l, v].concat(), [v, l].concat()); // alloc: Cold
        let at = |_: i64| PK_VERDICT;
        let sel: Vec<u32> = (0..1000u32).filter(|i| i % 3 != 1).collect(); // alloc: Cold
        let fl = PK_CODES;
        for (tab, d) in [(after.as_ptr(), 1), (before.as_ptr().wrapping_add(3), -1)] {
            assert_eq!(run_at(&mut [lane()], &[], (100, 900), tab, fl | PK_LEN | at(d), &[d]), Some(want(&mut (100..900))), "at {d}");
            let got = run_at(&mut [lane()], &sel, (10, 600), tab, fl | PK_LEN | at(d), &[d]);
            assert_eq!(got, Some(want(&mut sel[10..600].iter().map(|&i| i as usize))), "at {d}, selected");
        }
        let n = want(&mut (0..1000)).0;
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), after.as_ptr().wrapping_add(3), fl | at(0), &[0]), Some((n, 0)), "count only");
        assert_eq!(run_at(&mut [lane()], &sel, (0, sel.len() as u64), after.as_ptr(), fl | PK_LEN | at(1), &[1]), Some(want(&mut sel.iter().map(|&i| i as usize))), "the whole selection, entry by entry");
        let fl = PK_CODES | PK_LEN | at(1);
        let faulted = [l, [bits.as_ptr() as u64, faults.as_ptr() as u64, 0]].concat(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), faulted.as_ptr(), fl, &[1]), None, "a faulted entry");
        // D2074's tables the rows fill: only the window's read codes count
        let no67: Vec<u32> = (0..1000u32).filter(|&i| codes[i as usize] != 67).collect(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &no67, (0, no67.len() as u64), faulted.as_ptr(), fl, &[1]), Some(want(&mut no67.iter().map(|&i| i as usize))), "a faulted entry no row reads");
        let nobits = [l, [0, zero.as_ptr() as u64, 0]].concat(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), nobits.as_ptr(), fl, &[1]), None, "no bits");
        let mut w = [0u64; 128];
        let miss = Lane { ptr: w.as_mut_ptr() as *const u8, id: lane::id(lane::ID_VERDICT, 0), ..code_lane(&codes) };
        assert_eq!(run_at(&mut [lane(), miss], &[], (0, 1000), after.as_ptr(), fl, &[1]), None, "a miss under a verdict");
        assert_eq!(w, [0; 128], "its words untouched");
        for (sel, win) in [(&[][..], (100u64, 900u64)), (&sel[..], (10, 600))] {
            let mut w = [0u64; 128];
            let miss = Lane { ptr: w.as_mut_ptr() as *const u8, id: lane::id(lane::ID_VERDICT, 0), ..code_lane(&codes) };
            let rows: Vec<usize> = match sel { // alloc: Cold
                [] => (win.0 as usize..win.1 as usize).collect(),
                _ => sel[win.0 as usize..win.1 as usize].iter().map(|&i| i as usize).collect(),
            };
            let got = run_at(&mut [lane(), miss], sel, win, after.as_ptr(), fl | PK_COVERED, &[1]);
            assert_eq!(got, Some(want(&mut rows.iter().copied())), "a covered miss, {} selected", sel.len());
            let mut fill = [0u64; 128];
            rows.iter().filter(|&&i| pass(codes[i])).for_each(|&i| fill[i >> 6] |= 1 << (i & 63));
            assert_eq!(w, fill, "its kept rows' bits, {} selected", sel.len());
        }
        // D3179: a covered hit reads its selection without the verdicts (the selection is their survivors)
        let mut w = [u64::MAX; 128];
        let wp = w.as_mut_ptr() as *const u8;
        let hit = || Lane { ptr: wp, id: lane::id(lane::ID_VERDICT, 0), flags: lane::VERDICT_ALL_MATCH, ..code_lane(&codes) };
        let all = |rows: &mut dyn Iterator<Item = usize>| rows.map(|i| codes[i]).fold((0, 0), |(n, s), c| (n + 1, s + lens[c as usize]));
        let got = run_at(&mut [lane(), hit()], &sel, (10, 600), after.as_ptr(), fl | PK_COVERED, &[1]);
        assert_eq!(got, Some(all(&mut sel[10..600].iter().map(|&i| i as usize))), "a covered hit");
        let got = run_at(&mut [lane(), hit()], &sel, (10, 600), after.as_ptr(), fl, &[1]);
        assert_eq!(got, Some(want(&mut sel[10..600].iter().map(|&i| i as usize))), "a hit without PK_COVERED reads its verdicts");
    }

    /// BG-03 / B3-05: a signed per-code word table (any integer function of the column) sums exactly at its datum width,
    /// negative words included, past 64 bits; a kept row whose entry is faulted declines, a row a verdict drops never does.
    #[test]
    fn a_signed_word_table_sums_exactly() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let lane = || Lane { ncodes_or_scale: 97, ..code_lane(&codes) };
        // int4 datums as their sign-extended words, int2 as their raw low bits only, int8 near the edge
        let v4 = |c: u32| (c as i64 - 50) * 1_000_003;
        let w4: Vec<u64> = (0..97u32).map(|c| v4(c) as u64).collect(); // alloc: Cold
        let w2: Vec<u64> = (0..97u32).map(|c| u64::from(((c as i16) - 40) as u16)).collect(); // alloc: Cold
        let w8: Vec<u64> = (0..97u32).map(|c| (i64::MAX - c as i64) as u64).collect(); // alloc: Cold
        let (zero, mut faults) = ([0u64; 2], [0u64; 2]);
        let want = |f: &dyn Fn(u32) -> i128, keep: &dyn Fn(u32) -> bool| codes.iter().copied().filter(|&c| keep(c)).fold((0u64, 0i128), |(n, s), c| (n + 1, s + f(c)));
        let run_s = |tab: &[u64; 3], width: u64, fl: u64| run(&mut [lane()], &[], (0, 1000), tab, PK_CODES | PK_SIGNED | (width << PK_SIGNED_SHIFT) | fl);
        let (n, s) = want(&|c| i128::from(v4(c)), &|_| true);
        assert_eq!(run_s(&[w4.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS], 4, 0), Some((n, s as u64)), "int4");
        let (n, s) = want(&|c| i128::from((c as i16) - 40), &|_| true);
        assert_eq!(run_s(&[w2.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS], 2, 0), Some((n, s as u64)), "int2");
        // past 64 bits: the high word too
        // the lanes outlive the ctx that points at them
        let mut lanes = [lane()];
        let mut ctx = ctx_of(&mut lanes, &[]);
        let mut out = [0u64; answer::SIZE / 8];
        let tab = [w8.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS];
        // SAFETY: as `run_at`'s
        let r = unsafe { agg_percode_const(ctx.as_mut_ptr() as *mut Ctx, 0, 0, 1000, tab.as_ptr(), PK_CODES | PK_SIGNED | (8 << PK_SIGNED_SHIFT), core::ptr::null(), out.as_mut_ptr() as *mut u8) };
        let s = want(&|c| i128::from(i64::MAX - c as i64), &|_| true).1;
        assert_eq!((r, out[answer::SUM_LO / 8], out[answer::SUM_HI / 8]), (0, s as u64, (s >> 64) as u64), "an int8 sum past 64 bits");
        // a faulted entry some kept row reads declines; one whose rows a verdict drops does not
        faults[0] = 1 << 9;
        assert_eq!(run_s(&[w4.as_ptr() as u64, faults.as_ptr() as u64, BIND_FORM_WORDS], 4, 0), None, "a faulted entry");
        let mut but9 = [u64::MAX, u64::MAX];
        but9[0] &= !(1 << 9);
        let tabs = [[w4.as_ptr() as u64, faults.as_ptr() as u64, BIND_FORM_WORDS], [but9.as_ptr() as u64, zero.as_ptr() as u64, 0]].concat(); // alloc: Cold
        let (n, s) = want(&|c| i128::from(v4(c)), &|c| c != 9);
        let got = run_at(&mut [lane()], &[], (0, 1000), tabs.as_ptr(), PK_CODES | PK_SIGNED | (4 << PK_SIGNED_SHIFT) | PK_VERDICT, &[1]);
        assert_eq!(got, Some((n, s as u64)), "a faulted entry of a dropped code");
        assert_eq!(run_s(&[w4.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS], 3, 0), None, "an unknown width");
        assert_eq!(run_s(&[w4.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_IDX_BYTES], 4, 0), None, "not a word table");
    }

    /// BG-03: the verdicts of the column AND per code (each table anywhere in the out array), a fault in either declines;
    /// a comparison with one constant is a verdict of one code (`= c`) or of every code but one (`<> c`) like any other.
    #[test]
    fn verdicts_and_per_code() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let one = 6u32;
        let (p1, p2) = (|c: u32| c % 3 == 0, |c: u32| c % 2 == 0);
        let (mut b1, mut b2, zero, mut faults) = ([0u64; 2], [0u64; 2], [0u64; 2], [0u64; 2]);
        (0..97u32).filter(|&c| p1(c)).for_each(|c| b1[(c >> 6) as usize] |= 1 << (c & 63));
        (0..97u32).filter(|&c| p2(c)).for_each(|c| b2[(c >> 6) as usize] |= 1 << (c & 63));
        faults[0] = 1 << 12;
        let lens: Vec<u64> = (0..97u64).map(|c| c % 13 + 1).collect(); // alloc: Cold
        let lane = || Lane { ncodes_or_scale: 97, ..code_lane(&codes) };
        let want = |keep: &dyn Fn(u32) -> bool| codes.iter().copied().filter(|&c| keep(c)).fold((0, 0), |(n, s), c| (n + 1, s + lens[c as usize]));
        let (l, v1, v2) = ([lens.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS], [b1.as_ptr() as u64, zero.as_ptr() as u64, 0], [b2.as_ptr() as u64, zero.as_ptr() as u64, 0]);
        let tabs = [v1, l, v2].concat(); // alloc: Cold
        let at_l = tabs.as_ptr().wrapping_add(3);
        let fl = PK_CODES | PK_LEN | PK_VERDICT;
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), at_l, fl, &[-1, 1]), Some(want(&|c| p1(c) && p2(c))), "two verdicts");
        let (mut eq, mut ne) = ([0u64; 2], [u64::MAX; 2]);
        eq[0] |= 1 << one;
        ne[0] &= !(1 << one);
        let (veq, vne) = ([eq.as_ptr() as u64, zero.as_ptr() as u64, 0], [ne.as_ptr() as u64, zero.as_ptr() as u64, 0]);
        let consts = [v1, l, v2, veq, vne].concat(); // alloc: Cold
        let at_c = consts.as_ptr().wrapping_add(3);
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), at_c, fl, &[1, -1, 3]), Some(want(&|c| c != one && p1(c) && p2(c))), "two verdicts and <> c");
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), at_c, fl, &[-1, 2, 1]), Some(want(&|c| c == one && p1(c) && p2(c))), "= c and two verdicts");
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), at_c, fl, &[2]), Some(want(&|c| c == one)), "= c alone");
        // fault words that are the zero page: the pass reads none
        let zf = crate::dict::codes::zero_faults() as u64;
        let clean = [[b1.as_ptr() as u64, zf, 0], l, [b2.as_ptr() as u64, zf, 0]].concat(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), clean.as_ptr().wrapping_add(3), fl, &[-1, 1]), Some(want(&|c| p1(c) && p2(c))), "two verdicts, no fault words");
        let f2 = [b2.as_ptr() as u64, faults.as_ptr() as u64, 0];
        let faulted = [v1, l, f2].concat(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), faulted.as_ptr().wrapping_add(3), fl, &[-1, 1]), None, "a fault in the second verdict");
        let unbound = [v1, l, [0, zero.as_ptr() as u64, 0]].concat(); // alloc: Cold
        assert_eq!(run_at(&mut [lane()], &[], (0, 1000), unbound.as_ptr().wrapping_add(3), fl, &[-1, 1]), None, "an unbound second verdict");
    }

    /// Every length form, with and without a selection, equals the rows' own lengths. A count-only fold counts the window.
    /// A hit reads its selection as it is. A verdict lane still to fill (a miss), a lane that may hold a NULL, a lane that is
    /// not a decoded code lane and an unknown form decline, `out` unwritten.
    #[test]
    fn a_window_folds_its_rows_lengths() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let (bytes, chars) = (|c: u32| u64::from(c % 13 + 1) * 3, |c: u32| u64::from(c % 13 + 1));
        let mut idx = vec![0u8; 12 * 97 + 8]; // alloc: Cold
        let mut words = vec![0u64; 97]; // alloc: Cold
        let mut delta = vec![0u8; 12 * 97 + 8]; // alloc: Cold
        for c in 0..97u32 {
            let at = 12 * c as usize;
            idx[at..at + 4].copy_from_slice(&(bytes(c) as u32).to_le_bytes());
            idx[at + 4..at + 8].copy_from_slice(&(chars(c) as u32).to_le_bytes());
            delta[at..at + 4].copy_from_slice(&(bytes(c) as u32).to_le_bytes());
            delta[at + 4..at + 8].copy_from_slice(&((bytes(c) - chars(c)) as u32).to_le_bytes());
            words[c as usize] = bytes(c);
        }
        let sel: Vec<u32> = (0..1000u32).filter(|i| i % 3 != 1).collect(); // alloc: Cold
        let want = |rows: &mut dyn Iterator<Item = usize>, f: &dyn Fn(u32) -> u64| rows.map(|i| codes[i]).fold((0, 0), |(n, s), c| (n + 1, s + f(c)));
        let tabs: [([u64; 3], &dyn Fn(u32) -> u64); 4] = [
            ([idx.as_ptr() as u64, 0, BIND_FORM_IDX_BYTES], &bytes),
            ([idx.as_ptr() as u64, 0, BIND_FORM_IDX_CHARS], &chars),
            ([delta.as_ptr() as u64, 0, BIND_FORM_IDX_DELTA], &chars),
            ([words.as_ptr() as u64, 0, BIND_FORM_WORDS], &bytes),
        ];
        for (tab, f) in tabs.iter() {
            let fl = PK_CODES | PK_LEN;
            let mut l = [code_lane(&codes)];
            assert_eq!(run(&mut l, &[], (100, 900), tab, fl), Some(want(&mut (100..900), f)), "form {}", tab[2]);
            let got = run(&mut l, &sel, (10, 600), tab, fl);
            assert_eq!(got, Some(want(&mut sel[10..600].iter().map(|&i| i as usize), f)), "form {}, selected", tab[2]);
        }
        let (bt, fl) = (tabs[0].0, PK_CODES | PK_LEN);
        assert_eq!(run(&mut [code_lane(&codes)], &[], (0, 1000), &bt, PK_CODES), Some((1000, 0)), "count(col)");
        assert_eq!(run(&mut [], &sel, (3, 40), &bt, 0), Some((37, 0)), "count(*) alone reads no lane");
        let words8 = [0u64; 128];
        let verdict = |flags: u8| Lane { ptr: words8.as_ptr() as *const u8, id: lane::id(lane::ID_VERDICT, 0), flags, ..code_lane(&codes) };
        // a hit reads its selection as it is (D3179: the sparse stage's other rows' codes are stale); its words never written
        let selected = Some(want(&mut sel.iter().map(|&i| i as usize), &bytes));
        let hit = || [code_lane(&codes), verdict(lane::VERDICT_ALL_MATCH)];
        assert_eq!(run(&mut hit(), &sel, (0, sel.len() as u64), &bt, fl), selected, "the whole selection of a hit");
        assert_eq!(run(&mut [code_lane(&codes)], &sel, (0, sel.len() as u64), &bt, fl), selected, "the whole selection, no hit");
        assert_eq!(run(&mut hit(), &[], (0, 1000), &bt, fl), Some(want(&mut (0..1000), &bytes)), "a hit");
        assert_eq!(words8, [0; 128], "a hit's words are never written");
        let mut w = [0u64; 128];
        let miss = Lane { ptr: w.as_mut_ptr() as *const u8, ..verdict(0) };
        assert_eq!(run(&mut [code_lane(&codes), miss], &[], (0, 1000), &bt, fl), None, "a miss");
        assert_eq!(w, [0; 128], "a miss's words untouched");
        let nulls = Lane { valid: words8.as_ptr(), ..code_lane(&codes) };
        assert_eq!(run(&mut [nulls], &[], (0, 1000), &bt, fl), None, "a lane that may hold a NULL");
        assert!(run(&mut [Lane { flags: lane::FLAG_ALL_VALID, ..nulls }], &[], (0, 1000), &bt, fl).is_some(), "a bitmap flagged all valid");
        assert_eq!(run(&mut [Lane { kind: lane::KIND_BYVAL, ..code_lane(&codes) }], &[], (0, 1000), &bt, fl), None, "decoded values");
        assert_eq!(run(&mut [code_lane(&codes)], &[], (0, 1000), &[idx.as_ptr() as u64, 0, 9], fl), None, "an unknown form");
    }

    /// D3154: under `PK_LENMM` the fold answers the kept rows' least and greatest table value beside the count and the sum,
    /// plain and over a selection; a signed word table's values sign-extended; no kept row answers the identities; never
    /// beside `PK_MINMAX`, nor without a table.
    #[test]
    fn a_window_keeps_its_least_and_greatest_length() {
        let codes: Vec<u32> = (0..1000u32).map(|i| (i * 7919) % 97).collect(); // alloc: Cold
        let lane = || Lane { ncodes_or_scale: 97, ..code_lane(&codes) };
        let zero = [0u64; 2];
        let lens: Vec<u64> = (0..97u64).map(|c| (c * 31) % 89 + 2).collect(); // alloc: Cold
        let tab = [lens.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS];
        let run_lm = |sel: &[u32], win: (u64, u64), tab: &[u64; 3], flags: u64| -> Option<(u64, u64, i64, i64, u64)> {
            let mut lanes = [lane()];
            let mut ctx = ctx_of(&mut lanes, sel);
            let mut out = [0u64; answer::SIZE / 8];
            let vs = [0i64];
            // SAFETY: as `run_at`'s
            let r = unsafe { agg_percode_const(ctx.as_mut_ptr() as *mut Ctx, 0, win.0, win.1, tab.as_ptr(), flags, vs.as_ptr(), out.as_mut_ptr() as *mut u8) };
            (r == 0).then(|| (out[answer::NONNULL / 8], out[answer::SUM_LO / 8], out[answer::MIN / 8] as i64, out[answer::MAX / 8] as i64, out[answer::FLAGS / 8]))
        };
        let want = |rows: &mut dyn Iterator<Item = usize>, val: &dyn Fn(u32) -> i64| {
            rows.map(|i| codes[i]).fold((0u64, i64::MAX, i64::MIN), |(n, lo, hi), c| (n + 1, lo.min(val(c)), hi.max(val(c))))
        };
        let sel: Vec<u32> = (0..1000u32).filter(|i| i % 3 != 1).collect(); // alloc: Cold
        let fl = PK_CODES | PK_LEN | PK_LENMM;
        let len = |c: u32| lens[c as usize] as i64;
        let (n, lo, hi) = want(&mut (100..900), &len);
        let sum: u64 = (100..900).map(|i| lens[codes[i] as usize]).sum();
        assert_eq!(run_lm(&[], (100, 900), &tab, fl), Some((n, sum, lo, hi, 0)), "the count, the sum, the extremes, no ANSWER_MINMAX");
        let (n, lo, hi) = want(&mut sel[10..600].iter().map(|&i| i as usize), &len);
        let got = run_lm(&sel, (10, 600), &tab, fl).map(|r| (r.0, r.2, r.3));
        assert_eq!(got, Some((n, lo, hi)), "selected");
        // no kept row (an empty window): the identities (nonnull 0 folds neither)
        assert_eq!(run_lm(&sel, (7, 7), &tab, fl).map(|r| (r.0, r.2, r.3)), Some((0, i64::MAX, i64::MIN)));
        // a signed word table: int4 datums sign-extended, negatives the least
        let v4 = |c: u32| (c as i64 - 50) * 1_000_003;
        let w4: Vec<u64> = (0..97u32).map(|c| v4(c) as u64).collect(); // alloc: Cold
        let st = [w4.as_ptr() as u64, zero.as_ptr() as u64, BIND_FORM_WORDS];
        let (n, lo, hi) = want(&mut (0..1000), &v4);
        assert_eq!(run_lm(&[], (0, 1000), &st, PK_CODES | PK_SIGNED | (4 << PK_SIGNED_SHIFT) | PK_LENMM).map(|r| (r.0, r.2, r.3)), Some((n, lo, hi)), "signed words");
        assert_eq!(run_lm(&[], (0, 1000), &tab, PK_CODES | PK_LEN | PK_LENMM | PK_MINMAX), None, "beside PK_MINMAX");
        assert_eq!(run_lm(&[], (0, 1000), &tab, PK_CODES | PK_LENMM), None, "no table");
        assert_eq!(run_lm(&[], (0, 1000), &tab, PK_LEN | PK_LENMM), None, "no code lane read");
    }
}
