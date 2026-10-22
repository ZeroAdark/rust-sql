//! Every fold here merges as `Rec::fold` does: a table folds byte for byte as `HashAgg::fold_run` folds it.
use crate::ops::sort::topn::t3::{Plan, NPARAMS};
use crate::ops::sort::topn::t3_dense::{Area, Dense, AREA_PAIRS, AREA_SHIFT};
use gravity_abi::breaker::{float8_pl, prefetch_read, Candidates, HashAgg, Rec, RecordLayout, SlotKind, SubSplit, FOLD_OVERFLOW, FOLD_PF, LAYOUT_IMPLICIT_COUNT, LAYOUT_KEY_TIES, MAX_RECORD_KEYS, MAX_RECORD_SLOTS, MOVING};
use gravity_abi::hash::KeyHash;
use super::fold_jit;

/// `k`: an add's source mask; an extreme's order key (the source wins where `s ^ k < d ^ k`); a float sum's 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    pub off: u32,
    pub j: u32,
    pub k: u64,
}

const NO_OP: Op = Op { off: 0, j: 0, k: 0 };
pub const SIGN: u64 = 1 << 63;

/// Merges grouped by kind: a refusal ends the statement, so the order between the groups is free; the refusable ones
/// keep slot order.
#[derive(Clone, Copy)]
pub struct Fold {
    layout: RecordLayout,
    adds: [Op; MAX_RECORD_SLOTS],
    wides: [Op; MAX_RECORD_SLOTS],
    ext: [Op; MAX_RECORD_SLOTS],
    rest: [Op; MAX_RECORD_SLOTS],
    n: [u8; 4],
    implicit: Option<Op>,
    valid: Option<usize>,
    rec: usize,
    sent: usize,
    nw: usize,
    pw: usize,
}

impl Fold {
    /// `None` where a merge reads across slots (`LAYOUT_KEY_TIES`, a hosted state) or the layout is wide.
    pub fn of(l: &RecordLayout) -> Option<Fold> {
        let (nw, ns) = (l.nkeys(), l.nslots());
        if l.is_wide() || l.flags & LAYOUT_KEY_TIES != 0 || nw == 0 || nw > MAX_RECORD_KEYS || ns > MAX_RECORD_SLOTS {
            return None;
        }
        let implicit = l.flags & LAYOUT_IMPLICIT_COUNT != 0;
        let mut f = Fold {
            layout: *l,
            adds: [NO_OP; MAX_RECORD_SLOTS],
            wides: [NO_OP; MAX_RECORD_SLOTS],
            ext: [NO_OP; MAX_RECORD_SLOTS],
            rest: [NO_OP; MAX_RECORD_SLOTS],
            n: [0; 4],
            implicit: None,
            valid: l.has_validity().then_some(l.valid_off() as usize),
            rec: l.size() as usize,
            sent: l.sentinel_off() as usize,
            nw,
            pw: l.partition_keys(),
        };
        for j in 0..ns {
            let off = l.slot_off(j) as u32;
            let op = |off: u32, k: u64| Op { off, j: j as u32, k };
            match l.slot_kind(j) {
                SlotKind::Count if implicit && off == l.sentinel_off() as u32 => f.implicit = Some(op(off, 0)),
                SlotKind::Count => f.push(0, op(off, !MOVING)),
                SlotKind::SumI64 => f.push(0, op(off, !0)),
                SlotKind::SumI128 => f.push(1, op(off, 0)),
                SlotKind::AvgI128 => {
                    f.push(1, op(off, 0));
                    f.push(0, op(off + 16, !0));
                }
                SlotKind::MinI64 => f.push(2, op(off, SIGN)),
                SlotKind::MaxI64 => f.push(2, op(off, !SIGN)),
                SlotKind::MinCode => f.push(2, op(off, 0)),
                SlotKind::MaxCode => f.push(2, op(off, !0)),
                SlotKind::SumF64 => f.push(3, op(off, 1)),
                SlotKind::AggState | SlotKind::NumSideMin | SlotKind::NumSideMax => return None,
                _ => f.push(3, op(off, 0)),
            }
        }
        Some(f)
    }

    fn push(&mut self, g: usize, o: Op) {
        let at = self.n[g] as usize;
        match g {
            0 => self.adds[at] = o,
            1 => self.wides[at] = o,
            2 => self.ext[at] = o,
            _ => self.rest[at] = o,
        }
        self.n[g] += 1;
    }

    /// The hook's accumulator opens zeroed (`t3_dense::Area::group`): only slots that open at zero ride.
    pub fn dense(l: &RecordLayout) -> Option<Fold> {
        let f = Fold::of(l)?;
        (f.valid.is_none() && f.nw == 2 && f.n[2] == 0 && f.n[3] == 0).then_some(f)
    }

    /// The zero key's word sits past the table's last identity, inside the record space (`8 * nw + 8` bytes a record).
    fn ident(l: &RecordLayout) -> Option<(usize, usize, usize)> {
        let nw = l.nkeys();
        let fits = !l.is_wide() && (1..=MAX_RECORD_KEYS).contains(&nw) && l.size() as usize >= 8 * nw + 8;
        fits.then(|| (nw, l.partition_keys(), l.size() as usize))
    }

    pub fn adds(&self) -> &[Op] {
        &self.adds[..self.n[0] as usize]
    }

    pub fn wides(&self) -> &[Op] {
        &self.wides[..self.n[1] as usize]
    }

    pub fn extremes(&self) -> &[Op] {
        &self.ext[..self.n[2] as usize]
    }

    pub fn rest(&self) -> &[Op] {
        &self.rest[..self.n[3] as usize]
    }

    pub fn implicit(&self) -> Option<Op> {
        self.implicit
    }

    pub fn valid(&self) -> Option<usize> {
        self.valid
    }

    /// (record bytes, sentinel offset, key words, route words)
    pub fn shape(&self) -> (usize, usize, usize, usize) {
        (self.rec, self.sent, self.nw, self.pw)
    }

    pub fn layout(&self) -> &RecordLayout {
        &self.layout
    }

    /// false on a float overflow (`FOLD_OVERFLOW`).
    #[inline(always)]
    unsafe fn fold(&self, dst: *mut u8, src: *const u8) -> bool {
        let [na, nwd, ne, nr] = self.n.map(usize::from);
        let Some(vo) = self.valid else {
            for o in &self.adds[..na] {
                add(o, dst, src);
            }
            for o in &self.wides[..nwd] {
                wide(o, dst, src);
            }
            for o in &self.ext[..ne] {
                extreme(o, dst, src, true);
            }
            if let Some(o) = &self.implicit {
                implicit(o, dst, src);
            }
            for o in &self.rest[..nr] {
                if !self.refusable(o, dst, src, true) {
                    return false;
                }
            }
            return true;
        };
        let (dw, sw) = (dst.add(vo) as *mut u64, src.add(vo) as *const u64);
        let (dv, sv) = (*dw, *sw);
        let has = |v: u64, o: &Op| v >> o.j & 1 != 0;
        for o in self.adds[..na].iter().filter(|o| has(sv, o)) {
            add(o, dst, src);
        }
        for o in self.wides[..nwd].iter().filter(|o| has(sv, o)) {
            wide(o, dst, src);
        }
        for o in self.ext[..ne].iter().filter(|o| has(sv, o)) {
            extreme(o, dst, src, has(dv, o));
        }
        if let Some(o) = self.implicit.as_ref().filter(|o| has(sv, o)) {
            implicit(o, dst, src);
        }
        for o in self.rest[..nr].iter().filter(|o| has(sv, o)) {
            if !self.refusable(o, dst, src, has(dv, o)) {
                return false;
            }
        }
        *dw |= sv;
        true
    }

    #[inline(always)]
    pub(crate) unsafe fn fold_first(&self, dst: *mut u8, src: *const u8, r: usize) {
        if r == 0 {
            return;
        }
        // each group is in slot order: the slots below `r` are a prefix
        let first = |o: &&Op| (o.j as usize) < r;
        for o in self.adds[..self.n[0] as usize].iter().take_while(first) {
            add(o, dst, src);
        }
        for o in self.wides[..self.n[1] as usize].iter().take_while(first) {
            wide(o, dst, src);
        }
        if let Some(o) = self.implicit.as_ref().filter(first) {
            implicit(o, dst, src);
        }
    }

    #[inline(always)]
    unsafe fn refusable(&self, o: &Op, dst: *mut u8, src: *const u8, dvalid: bool) -> bool {
        if o.k == 0 {
            return self.slot(o.j as usize, dst, src);
        }
        let (d, s) = words(o, dst, src);
        if !dvalid {
            *d = *s;
            return true;
        }
        match float8_pl(f64::from_bits(*d), f64::from_bits(*s)) {
            Ok(v) => {
                *d = v.to_bits();
                true
            }
            Err(_) => false,
        }
    }

    /// Out of line: one copy for every key word count.
    #[inline(never)]
    unsafe fn slot(&self, j: usize, dst: *mut u8, src: *const u8) -> bool {
        Rec::at(dst, &self.layout).fold_slot(j, &Rec::at(src as *mut u8, &self.layout)).is_ok()
    }

    /// # Safety
    /// `FoldFn`'s contract.
    pub unsafe fn run(&self, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64 {
        match self.nw {
            1 => run::<1>(t, recs, nrecs, split, cand, self),
            2 => run::<2>(t, recs, nrecs, split, cand, self),
            3 => run::<3>(t, recs, nrecs, split, cand, self),
            4 => run::<4>(t, recs, nrecs, split, cand, self),
            5 => run::<5>(t, recs, nrecs, split, cand, self),
            6 => run::<6>(t, recs, nrecs, split, cand, self),
            7 => run::<7>(t, recs, nrecs, split, cand, self),
            _ => run::<8>(t, recs, nrecs, split, cand, self),
        }
    }

    unsafe fn run_ident<H: IdentHook>(&self, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, hook: &H) -> i64 {
        debug_assert_eq!(self.nw, 2, "the dense hook's pair");
        run_ident::<2, H>(t, recs, nrecs, split, hook, self)
    }
}

#[inline(always)]
unsafe fn words(o: &Op, dst: *mut u8, src: *const u8) -> (*mut u64, *const u64) {
    (dst.add(o.off as usize) as *mut u64, src.add(o.off as usize) as *const u64)
}

#[inline(always)]
unsafe fn add(o: &Op, dst: *mut u8, src: *const u8) {
    let (d, s) = words(o, dst, src);
    *d = (*d).wrapping_add(*s & o.k);
}

#[inline(always)]
unsafe fn wide(o: &Op, dst: *mut u8, src: *const u8) {
    let (d, s) = words(o, dst, src);
    let v = ((*d as u128) | (*d.add(1) as u128) << 64).wrapping_add((*s as u128) | (*s.add(1) as u128) << 64);
    *d = v as u64;
    *d.add(1) = (v >> 64) as u64;
}

#[inline(always)]
unsafe fn extreme(o: &Op, dst: *mut u8, src: *const u8, dvalid: bool) {
    let (d, s) = words(o, dst, src);
    if !dvalid || *s ^ o.k < *d ^ o.k {
        *d = *s;
    }
}

#[inline(always)]
unsafe fn implicit(o: &Op, dst: *mut u8, src: *const u8) {
    let (d, s) = words(o, dst, src);
    *d = (*d as i64).wrapping_add(((*s & !MOVING) as i64 - 1).max(0)) as u64;
}

/// Perf: inline moves for each record size up to 16 words, no call (one per new group).
#[inline(always)]
unsafe fn copy_rec(d: *mut u8, s: *const u8, rec: usize) {
    macro_rules! sized {
        ($($b:literal)*) => {
            match rec {
                $($b => core::ptr::copy_nonoverlapping(s, d, $b),)*
                _ => core::ptr::copy_nonoverlapping(s, d, rec),
            }
        };
    }
    sized!(16 24 32 40 48 56 64 72 80 88 96 104 112 120 128)
}

#[inline(always)]
unsafe fn hash<const NW: usize>(p: *const u8) -> KeyHash {
    KeyHash::of(core::slice::from_raw_parts(p as *const u64, NW))
}

#[inline(always)]
unsafe fn route<const NW: usize>(h: KeyHash, src: *const u8, pw: usize) -> KeyHash {
    if pw == NW { h } else { KeyHash::of(core::slice::from_raw_parts(src as *const u64, pw)) }
}

#[inline(always)]
unsafe fn same<const NW: usize>(a: *const u8, b: *const u8) -> bool {
    same_from::<NW>(a, b, 0)
}

/// Words `from..NW` equal.
#[inline(always)]
unsafe fn same_from<const NW: usize>(a: *const u8, b: *const u8, from: usize) -> bool {
    let (a, b) = (a as *const u64, b as *const u64);
    let mut k = from;
    while k < NW {
        if *a.add(k) != *b.add(k) {
            return false;
        }
        k += 1;
    }
    true
}

/// The identity fold's hook: the group of a pair's first key word under a dense domain `[lo, lo + span)`.
pub(crate) trait IdentHook {
    fn lo(&self) -> i64;
    fn span(&self) -> u64;
    /// Leading delivered records of the run already noted (a re-read of the stream after a fill).
    fn quiet(&self) -> usize;
    /// A key outside the domain: the record is kept out of the table (counted as folded).
    unsafe fn bad(&self);
    /// Pair `src` of group index `ri` after its identity probe, `new` when the table opened it.
    unsafe fn note(&self, ri: u64, src: *const u8, new: bool, f: &Fold);
    /// The delivered index of the record met with the table full.
    unsafe fn filled(&self, i: usize);
}

/// The fill a probe loop stops at: 7/8 of the slots, and never the last one (D282: under 8 slots `cap / 8` is 0, a table
/// filled to its last slot left the next new key's probe no empty slot to stop at, and it spun instead of returning full).
#[inline(always)]
pub(crate) const fn full_at(cap: u32) -> u32 {
    let slack = cap / 8;
    cap - if slack == 0 { 1 } else { slack }
}

/// The identity fold (lower.md §6.12.8, kernel-floors.md §1.2): under a dense group index the table's slots are
/// dead (the reduce reads the index, never the table), so the table holds identities only — `NW` words per slot
/// at a stride of `8 * NW` over the same zeroed space (a third of the pair record's footprint for a two-key pair),
/// word 0 the group's dense index plus one (zero = empty), the rest the pair's other key words; the hook counts
/// and folds (`note`) at the insert, while the record is in L1. The first `quiet` records insert without a note.
/// A full table returns the fill at the record it met (`filled`); nothing is unwound: the finer re-read of the
/// same stream keeps the records before that position quiet.
unsafe fn run_ident<const NW: usize, H: IdentHook>(t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, hook: &H, f: &Fold) -> i64 {
    let (rec, pw) = (f.rec, f.pw);
    let base = (*t).base_ptr();
    let mask = (*t).cap as usize - 1;
    let full_at = full_at((*t).cap);
    let (cb, at) = (*t).home_bits();
    let mut len = (*t).len;
    let (lo, span, quiet) = (hook.lo(), hook.span(), hook.quiet());
    let mut ring = [KeyHash::word(0); FOLD_PF];
    for (i, r) in ring.iter_mut().enumerate().take(nrecs.min(FOLD_PF)) {
        *r = hash::<NW>(recs.add(i * rec));
        prefetch_read(base.add(r.home(cb, at) * 8 * NW));
    }
    let mut folded = 0i64;
    for i in 0..nrecs {
        let h = ring[i % FOLD_PF];
        if i + FOLD_PF < nrecs {
            let h2 = hash::<NW>(recs.add((i + FOLD_PF) * rec));
            ring[i % FOLD_PF] = h2;
            prefetch_read(base.add(h2.home(cb, at) * 8 * NW));
        }
        let src = recs.add(i * rec);
        // perf: the route's hash only under a sub-split (a call LLVM does not sink into `keeps`)
        if split.depth != 0 && !split.keeps(route::<NW>(h, src, pw)) {
            continue;
        }
        let ri = (*(src as *const i64)).wrapping_sub(lo) as u64;
        if ri >= span {
            hook.bad();
            folded += 1;
            continue;
        }
        let id = ri + 1;
        let mut j = h.home(cb, at);
        loop {
            let d = base.add(j * 8 * NW) as *mut u64;
            let w0 = *d;
            if w0 == 0 {
                if len >= full_at {
                    (*t).len = len;
                    hook.filled(i);
                    return -1 - folded;
                }
                len += 1;
                *d = id;
                let s = src as *const u64;
                let mut k = 1;
                while k < NW {
                    *d.add(k) = *s.add(k);
                    k += 1;
                }
                if i >= quiet {
                    hook.note(ri, src, true, f);
                }
                break;
            }
            if w0 == id && same_from::<NW>(d as *const u8, src, 1) {
                if i >= quiet {
                    hook.note(ri, src, false, f);
                }
                break;
            }
            j = (j + 1) & mask;
        }
        folded += 1;
    }
    (*t).len = len;
    folded
}

struct Riding<'a> {
    area: &'a Area,
    r: usize,
}

impl IdentHook for Riding<'_> {
    fn lo(&self) -> i64 {
        self.area.lo()
    }

    fn span(&self) -> u64 {
        self.area.span()
    }

    fn quiet(&self) -> usize {
        self.area.quiet()
    }

    unsafe fn bad(&self) {
        self.area.bad()
    }

    #[inline(always)]
    unsafe fn note(&self, ri: u64, src: *const u8, new: bool, f: &Fold) {
        let g = self.area.group(ri);
        *g.add(AREA_PAIRS) += new as u64;
        f.fold_first((g as *mut u8).add(AREA_SHIFT), src, self.r);
    }

    unsafe fn filled(&self, i: usize) {
        self.area.filled(i)
    }
}

unsafe fn run<const NW: usize>(t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates, f: &Fold) -> i64 {
    let (rec, sent, pw) = (f.rec, f.sent, f.pw);
    let base = (*t).base_ptr();
    let mask = (*t).cap as usize - 1;
    let full_at = full_at((*t).cap);
    let (cb, at) = (*t).home_bits();
    let mut len = (*t).len;
    let mut ring = [KeyHash::word(0); FOLD_PF];
    for (i, r) in ring.iter_mut().enumerate().take(nrecs.min(FOLD_PF)) {
        *r = hash::<NW>(recs.add(i * rec));
        prefetch_read(base.add(r.home(cb, at) * rec));
    }
    let mut folded = 0i64;
    for i in 0..nrecs {
        let h = ring[i % FOLD_PF];
        if i + FOLD_PF < nrecs {
            let h2 = hash::<NW>(recs.add((i + FOLD_PF) * rec));
            ring[i % FOLD_PF] = h2;
            prefetch_read(base.add(h2.home(cb, at) * rec));
        }
        let src = recs.add(i * rec);
        if split.depth != 0 && !split.keeps(route::<NW>(h, src, pw)) {
            continue;
        }
        let mut j = h.home(cb, at);
        loop {
            let d = base.add(j * rec);
            if *(d.add(sent) as *const u64) == 0 {
                if len >= full_at {
                    (*t).len = len;
                    return -1 - folded;
                }
                len += 1;
                copy_rec(d, src, rec);
                break;
            }
            if same::<NW>(d, src) {
                if !f.fold(d, src) {
                    (*t).len = len;
                    return FOLD_OVERFLOW;
                }
                break;
            }
            j = (j + 1) & mask;
        }
        folded += 1;
        (*cand).note(base.add(j * rec), j);
    }
    (*t).len = len;
    folded
}

impl IdentHook for Dense {
    fn lo(&self) -> i64 {
        self.lo()
    }

    fn span(&self) -> u64 {
        self.span()
    }

    fn quiet(&self) -> usize {
        Dense::quiet(self)
    }

    unsafe fn bad(&self) {
        // D597: a flag the reduce refuses on by name, never a record dropped silently
        self.set_bad()
    }

    #[inline(always)]
    unsafe fn note(&self, ri: u64, src: *const u8, new: bool, f: &Fold) {
        Dense::note(self, ri, src, new, f)
    }

    unsafe fn filled(&self, i: usize) {
        Dense::filled(self, i)
    }
}

/// `full`: the partition words are all the key words, so the route is the probe's hash.
macro_rules! by_width {
    ($f:ident, $nw:expr, $full:expr, ($($a:expr),*)) => {
        match ($nw, $full) {
            (1, true) => $f::<1, true>($($a),*),
            (1, false) => $f::<1, false>($($a),*),
            (2, true) => $f::<2, true>($($a),*),
            (2, false) => $f::<2, false>($($a),*),
            (3, true) => $f::<3, true>($($a),*),
            (3, false) => $f::<3, false>($($a),*),
            (4, true) => $f::<4, true>($($a),*),
            (4, false) => $f::<4, false>($($a),*),
            (5, true) => $f::<5, true>($($a),*),
            (5, false) => $f::<5, false>($($a),*),
            (6, true) => $f::<6, true>($($a),*),
            (6, false) => $f::<6, false>($($a),*),
            (7, true) => $f::<7, true>($($a),*),
            (7, false) => $f::<7, false>($($a),*),
            (_, true) => $f::<8, true>($($a),*),
            (_, false) => $f::<8, false>($($a),*),
        }
    };
}

/// The plain identity fold (kernel-floors.md §2.1): `count(DISTINCT x)` alone reads the table's `len` only, so the
/// table holds identities — `NW` words per slot at a stride of `8 * NW`, an all-zero slot empty; the all-zero key is
/// the word past the last slot (`Fold::ident`) and counts in `len` without a slot. The fill protocol is `run_ident`'s.
#[inline(never)]
unsafe fn run_plain<const NW: usize, const FULL: bool>(t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, rec: usize, pw: usize) -> i64 {
    let base = (*t).base_ptr();
    let cap = (*t).cap as usize;
    let mask = cap - 1;
    let full_at = full_at((*t).cap);
    let (cb, at) = (*t).home_bits();
    let mut len = (*t).len;
    let zero = base.add(cap * 8 * NW) as *mut u64;
    let mut ring = [KeyHash::word(0); FOLD_PF];
    for (i, r) in ring.iter_mut().enumerate().take(nrecs.min(FOLD_PF)) {
        *r = hash::<NW>(recs.add(i * rec));
        prefetch_read(base.add(r.home(cb, at) * 8 * NW));
    }
    let mut folded = 0i64;
    for i in 0..nrecs {
        let h = ring[i % FOLD_PF];
        if i + FOLD_PF < nrecs {
            let h2 = hash::<NW>(recs.add((i + FOLD_PF) * rec));
            ring[i % FOLD_PF] = h2;
            prefetch_read(base.add(h2.home(cb, at) * 8 * NW));
        }
        let src = recs.add(i * rec);
        if split.depth != 0 && !split.keeps(if FULL { h } else { route::<NW>(h, src, pw) }) {
            continue;
        }
        if blank::<NW>(src) {
            len += (*zero == 0) as u32;
            *zero = 1;
            folded += 1;
            continue;
        }
        let mut j = h.home(cb, at);
        loop {
            let d = base.add(j * 8 * NW);
            if blank::<NW>(d) {
                if len >= full_at {
                    (*t).len = len;
                    return -1 - folded;
                }
                len += 1;
                core::ptr::copy_nonoverlapping(src, d, 8 * NW);
                break;
            }
            if same::<NW>(d, src) {
                break;
            }
            j = (j + 1) & mask;
        }
        folded += 1;
    }
    (*t).len = len;
    folded
}

/// `run_plain` over slices of `stride`-byte records whose key words lead (road dict.entries' runs: the 16 B `[lo, hi]`
/// identities, `ident_iov`): one prefetch ring across the slices, the input prefetched 256 B ahead of the ring's cursor
/// and each next slice's head when the cursor enters a slice.
/// Perf: out of line; inlined into `ident_iov`'s dispatch the loops reload the table's bounds from the stack (+10 %).
#[inline(never)]
unsafe fn run_plain_iov<const NW: usize, const FULL: bool>(t: *mut HashAgg, sl: &[(*const u8, usize)], stride: usize, split: SubSplit, pw: usize) -> i64 {
    let base = (*t).base_ptr();
    let cap = (*t).cap as usize;
    let mask = cap - 1;
    let full_at = full_at((*t).cap);
    let (cb, at) = (*t).home_bits();
    let mut len = (*t).len;
    let zero = base.add(cap * 8 * NW) as *mut u64;
    // the ring's cursor: its next record, the records left in its slice, the next slice
    let (mut la, mut left, mut ns) = (core::ptr::null::<u8>(), 0usize, 0usize);
    let mut ahead = || -> Option<*const u8> {
        while left == 0 {
            let &(p, n) = sl.get(ns)?;
            (la, left, ns) = (p, n, ns + 1);
            if let Some(&(q, _)) = sl.get(ns) {
                prefetch_read(q);
                prefetch_read(q.wrapping_add(64));
            }
        }
        let p = la;
        la = la.wrapping_add(stride);
        left -= 1;
        prefetch_read(p.wrapping_add(256));
        Some(p)
    };
    let mut ring = [KeyHash::word(0); FOLD_PF];
    for r in ring.iter_mut() {
        let Some(p) = ahead() else { break };
        *r = hash::<NW>(p);
        prefetch_read(base.add(r.home(cb, at) * 8 * NW));
    }
    let (mut k, mut folded) = (0usize, 0i64);
    for &(p0, n) in sl {
        for i in 0..n {
            let src = p0.add(i * stride);
            let h = ring[k % FOLD_PF];
            if let Some(p) = ahead() {
                let h2 = hash::<NW>(p);
                ring[k % FOLD_PF] = h2;
                prefetch_read(base.add(h2.home(cb, at) * 8 * NW));
            }
            k += 1;
            if split.depth != 0 && !split.keeps(if FULL { h } else { route::<NW>(h, src, pw) }) {
                continue;
            }
            if blank::<NW>(src) {
                len += (*zero == 0) as u32;
                *zero = 1;
                folded += 1;
                continue;
            }
            let mut j = h.home(cb, at);
            loop {
                let d = base.add(j * 8 * NW);
                if blank::<NW>(d) {
                    if len >= full_at {
                        (*t).len = len;
                        return -1 - folded;
                    }
                    len += 1;
                    core::ptr::copy_nonoverlapping(src, d, 8 * NW);
                    break;
                }
                if same::<NW>(d, src) {
                    break;
                }
                j = (j + 1) & mask;
            }
            folded += 1;
        }
    }
    (*t).len = len;
    folded
}

#[inline(always)]
unsafe fn blank<const NW: usize>(p: *const u8) -> bool {
    let p = p as *const u64;
    let mut w = 0;
    let mut k = 0;
    while k < NW {
        w |= *p.add(k);
        k += 1;
    }
    w == 0
}

/// A slice form of the plain identity fold (`ident_iov`).
pub type IovRunFn = unsafe fn(*mut HashAgg, &[(*const u8, usize)], usize, SubSplit) -> i64;

/// Road dict.entries' runs (hosts granule_source/entry_runs.rs): slices of `stride`-byte records whose key
/// words lead (the runs' 16 B `[lo, hi]`, no count: an identity counts once) folded into the plain distinct owner's pair
/// table `t`. Its identity fold over the slices (one prefetch ring across them); a table of any other layout
/// takes each record opened as its own (`Rec::open`: the keys, a count of 1) through the generic fold. The records folded,
/// `-1 - folded` on a full table, or `FOLD_OVERFLOW` where the generic fold raises one (`FoldFn`'s contract).
/// # Safety
/// Each slice holds whole records of `stride >= 8 * nkeys` bytes, read-only for the call; `t` is the owner's live table.
pub unsafe fn ident_iov(t: *mut HashAgg, sl: &[(*const u8, usize)], stride: usize, split: SubSplit) -> i64 {
    if let Some((nw, pw, _)) = Fold::ident(&(*t).layout) {
        return by_width!(run_plain_iov, nw, pw == nw, (t, sl, stride, split, pw));
    }
    let l = (*t).layout;
    let (nk, mut img, mut cand) = (l.nkeys(), [0u64; 32], Candidates::none());
    if l.size() as usize > 8 * img.len() {
        return -1;
    }
    let mut folded = 0i64;
    for &(p, n) in sl {
        for i in 0..n {
            let r = Rec::at(img.as_mut_ptr() as *mut u8, &l);
            r.clear();
            r.open(core::slice::from_raw_parts(p.add(i * stride) as *const u64, nk));
            let v = (*t).fold_run(img.as_ptr() as *const u8, 1, split, &mut cand);
            if v == FOLD_OVERFLOW {
                return v;
            }
            if v < 0 {
                return -1 - folded;
            }
            folded += v;
        }
    }
    folded
}

pub fn generated(l: &RecordLayout) -> bool {
    Fold::of(l).is_some()
}

pub fn ident_folds(l: &RecordLayout) -> bool {
    Fold::ident(l).is_some()
}

pub fn dense_folds(l: &RecordLayout) -> bool {
    Fold::dense(l).is_some()
}

/// # Safety
/// `FoldFn`'s contract; `area` laid over the owner's region for this table.
pub unsafe fn area_fold(t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, area: &Area, f: &Fold, r: usize) -> i64 {
    f.run_ident(t, recs, nrecs, split, &Riding { area, r })
}

/// `OwnerFns.fold` of the distinct family: the pair table's shape with the dense hook when the plan's dense
/// area is live for this table (`Dense::at_table`), else `owner_fold`. The driver re-reads a filled partition's
/// stream from its start (`merge.rs fold_sub`: once at the same depth, then under the finer split); the area's
/// `Fills` keep the records noted before the fill quiet on the re-read.
/// # Safety
/// `FoldFn`'s contract.
pub unsafe fn owner_fold_dense(params: *const u64, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64 {
    if let Some(mut d) = Dense::at_table(params, t) {
        if let Some(f) = Fold::dense(&(*t).layout) {
            let mut fill = 0u64;
            d.stream(split, &mut fill);
            let v = f.run_ident(t, recs, nrecs, split, &d);
            d.advance(split, nrecs, v, fill);
            return v;
        }
    }
    if plain_owner(params, t) {
        if Fold::ident(&(*t).layout).is_some() {
            return owner_fold_ident(params, t, recs, nrecs, split, cand);
        }
    }
    owner_fold(params, t, recs, nrecs, split, cand)
}

/// The hand plan's `count(DISTINCT x)` alone over its pair table: `distinct_reduce` sums `len`, nothing reads the slots.
unsafe fn plain_owner(params: *const u64, t: *const HashAgg) -> bool {
    Plan::from_words(core::slice::from_raw_parts(params, NPARAMS)).is_some_and(|pl| pl.nkeys == 1 && !pl.gmark && !pl.two_pass && (*t).layout == pl.layout())
}

/// `OwnerFns.fold` of exec's plain distinct owner (`owner.rs` `ident_form`: the reduce reads `len`): the pair
/// table's identity fold, else `owner_fold`.
/// # Safety
/// `FoldFn`'s contract.
pub unsafe fn owner_fold_ident(params: *const u64, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64 {
    let Some((nw, pw, rec)) = Fold::ident(&(*t).layout) else { return owner_fold(params, t, recs, nrecs, split, cand) };
    by_width!(run_plain, nw, pw == nw, (t, recs, nrecs, split, rec, pw))
}

/// `OwnerFns.fold`. The compiled fold has no split filter: a sub-split takes the counted loops.
/// # Safety
/// `FoldFn`'s contract.
pub unsafe fn owner_fold(_params: *const u64, t: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64 {
    let key = fold_jit::key(&(*t).layout);
    if split.depth == 0 {
        if let Some((_pin, e)) = fold_jit::find(&key).and_then(|p| p.entry().map(|e| (p, e))) {
            return fold_jit::run(e, t, recs, nrecs, cand);
        }
    }
    match Fold::of(&(*t).layout) {
        Some(f) if split.depth == 0 => match fold_jit::get(&f, &key).and_then(|p| p.entry().map(|e| (p, e))) {
            Some((_pin, e)) => fold_jit::run(e, t, recs, nrecs, cand),
            None => f.run(t, recs, nrecs, split, cand),
        },
        Some(f) => f.run(t, recs, nrecs, split, cand),
        None if crate::ops::agg::trans::hosted::handle_slot(&(*t).layout).is_some() => {
            (*t).fold_run_with(recs, nrecs, split, &mut *cand, &mut crate::ops::agg::trans::hosted::owner_combine)
        }
        None => (*t).fold_run(recs, nrecs, split, &mut *cand),
    }
}
