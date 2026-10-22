//! The distinct family's dense index (§2.2ad): one int group key's exact domain `[lo, hi]` (the resident
//! zone extrema, read at admission into the distinct plan's `P_OUTVAL` word) lets the owner FOLD count at
//! insert (`fold::owner_fold_dense`): a pair's group is `key - lo` into a `u32` slot table over a compact array of
//! accumulators laid BEFORE the partition table (a records'-size table takes all the space left); the round's end appends.
//! It sits in the family's raw half because the index is raw-pointer work over the owner's region; topn/mod.rs
//! re-exports it as `ops::sort::topn::t3_dense`, the path its readers name. It reads the t3 plan's words beside it.

use crate::ops::sort::topn::t3::{Plan, K_INT, NPARAMS, P_OUTVAL};
use gravity_abi::breaker::{HashAgg, Rec, RecordLayout, SubSplit, T1, DENSE_TABLE_FLOOR as FLOOR, REDUCE_FLUSH};
use gravity_abi::hash::KeyHash;

const LO_BITS: u32 = 40;
const PRESENT: u64 = 1 << 63;
const LO_MASK: u64 = (1 << LO_BITS) - 1;
/// The widest domain a word encodes: its 23 span bits (bits 40..63 hold `span - 1`). B04 / CAP-21: the word's own capacity,
/// not a cost bound; whether a dense area of a span pays is program cost `owner::dense_pays`, read by exec's keyed owner at
/// its bind and for its region floor (a span past this, or one that does not pay, leaves the pair path).
pub const SPAN_MAX: u64 = 1 << (63 - LO_BITS);

/// Bit 63 set, bits 40..63 `span - 1`, bits 0..40 `lo` (two's complement); 0 when it does not encode.
pub const fn word(lo: i64, hi: i64) -> u64 {
    if lo > hi {
        return 0;
    }
    let span = hi as i128 - lo as i128 + 1;
    let (lo_min, lo_max) = (-(1i64 << (LO_BITS - 1)), (1i64 << (LO_BITS - 1)) - 1);
    if span > SPAN_MAX as i128 || lo < lo_min || lo > lo_max {
        return 0;
    }
    PRESENT | ((span as u64 - 1) << LO_BITS) | (lo as u64 & LO_MASK)
}

pub const fn domain(w: u64) -> Option<(i64, u32)> {
    if w & PRESENT == 0 {
        return None;
    }
    let lo = (((w & LO_MASK) << (64 - LO_BITS)) as i64) >> (64 - LO_BITS);
    let span = ((w >> LO_BITS) & (PRESENT - 1) >> LO_BITS) as u32 + 1;
    Some((lo, span))
}

pub const fn bytes(span: u32, rec: u32) -> usize {
    idx_bytes(span) + span as usize * rec as usize
}

const fn idx_bytes(span: u32) -> usize {
    (HDR + 4 * span as usize + 7) & !7
}

/// `[n: u32][resume: u32][Fills][BAD: the fold met a key outside the domain (D597)]`, the slot table, the records; then
/// `MARKER` right before the partition table: the area's bytes, 0 = no room.
const HDR: usize = 16 + 8 * Fills::WORDS;
const BAD: usize = 8 + 8 * Fills::WORDS;
pub const MARKER: usize = 8;

/// A partition fold's stream position and the position its table filled at per split depth, `[pos, fill[0..DEPTHS]]`, in exec's
/// area header and the `Dense` header: the driver re-reads a filled partition from its start; the records noted before a fill stay
/// quiet. One slot per depth the split can reach (D471: four slots aliased depth 4 onto depth 3 once D445 let the split past it).
pub struct Fills;

impl Fills {
    /// The split depths a table can reach (the hash home's `Depth::LEVELS`): one fill slot per depth.
    pub const DEPTHS: usize = gravity_abi::hash::Depth::LEVELS;
    pub const WORDS: usize = 1 + Self::DEPTHS;

    /// The run's leading records noted already: the coarser depths' fills (this depth's on a re-read) past the position. # Safety — `WORDS` words.
    #[inline(always)]
    pub unsafe fn quiet(w: *const u64, split: SubSplit) -> usize {
        let mut thr = 0u64;
        // `depth + reread` is at most `DEPTHS` (a same-depth re-read at the deepest depth); the clamp never binds
        for d in 0..(split.depth as usize + split.reread as usize).min(Self::DEPTHS) {
            thr = thr.max(*w.add(1 + d));
        }
        thr.saturating_sub(*w) as usize
    }

    /// After a run of `nrecs`: `v` the fold's return, `fill` the delivered index it filled at (`v < 0`; recorded, the stream rewound). # Safety — `WORDS` words.
    #[inline(always)]
    pub unsafe fn advance(w: *mut u64, split: SubSplit, nrecs: usize, v: i64, fill: u64) {
        if v < 0 {
            // `depth < DEPTHS` (`merge.rs` splits while `Depth::new` holds); the clamp never binds
            *w.add(1 + (split.depth as usize).min(Self::DEPTHS - 1)) = *w + fill;
            *w = 0;
        } else {
            *w += nrecs as u64;
        }
    }

    /// The table's end (its reduce): the next table's stream starts at 0. # Safety — `w` the `WORDS` words.
    pub unsafe fn reset(w: *mut u64) {
        *w = 0;
    }
}

pub struct Dense {
    base: *mut u8,
    lo: i64,
    span: u32,
    acc: RecordLayout,
    /// The plan's slots (the pair record's explicit ones), folded at their offsets; the layout's implicit count is not one.
    nslots: usize,
    quiet: usize,
    fill: *mut u64,
}

impl Dense {
    /// The plan's dense form: one int group key without marks, the pair fold's NOT NULL layouts, a domain word. # Safety — `NPARAMS` plan words.
    unsafe fn form(params: *const u64) -> Option<(i64, u32, RecordLayout, usize)> {
        let pl = Plan::from_words(core::slice::from_raw_parts(params, NPARAMS))?;
        if pl.nkeys != 2 || pl.key_kind[0] != K_INT || pl.gmark || pl.two_pass {
            return None;
        }
        let (acc, pair) = (pl.layout_of(1, 1), pl.layout());
        if acc.has_validity() || pair.has_validity() {
            return None;
        }
        // the fold at the pair layout's offsets (`Fold::fold_first`): the pair count right after the key, every plan slot where the pair record has it
        if acc.slot_off[0] != 8 || (0..pl.nslots).any(|j| acc.slot_off[j + 1] != pair.slot_off[j]) {
            return None;
        }
        let (lo, span) = domain(*params.add(P_OUTVAL))?;
        Some((lo, span, acc, pl.nslots))
    }

    /// The plan's dense area at `start`, when the plan takes one and it fits `space` beside `FLOOR` of table space and its marker.
    pub(crate) unsafe fn of(params: *const u64, start: *mut u8, space: usize) -> Option<Dense> {
        let (lo, span, acc, nslots) = Dense::form(params)?;
        let b = bytes(span, acc.size());
        if b + MARKER + FLOOR > space {
            return None;
        }
        Some(Dense { base: start, lo, span, acc, nslots, quiet: 0, fill: core::ptr::null_mut() })
    }

    /// The table space at `start` laid: the area (opened) + its marker, or the marker alone (0: the form without the room); the bytes taken.
    pub(crate) unsafe fn lay(params: *const u64, start: *mut u8, space: usize) -> usize {
        if Dense::form(params).is_none() {
            return 0;
        }
        match Dense::of(params, start, space) {
            Some(d) => {
                d.open();
                let b = d.bytes();
                *(start.add(b) as *mut u64) = b as u64;
                b + MARKER
            }
            None => {
                *(start as *mut u64) = 0;
                MARKER
            }
        }
    }

    /// From a partition table at `lay`'s end; `None` without the form or the room (the pair mode). # Safety — a live table.
    pub(crate) unsafe fn at_table(params: *const u64, t: *const HashAgg) -> Option<Dense> {
        let (lo, span, acc, nslots) = Dense::form(params)?;
        let b = *(t as *const u64).sub(1) as usize;
        (b != 0).then(|| Dense { base: (t as *mut u8).sub(MARKER + b), lo, span, acc, nslots, quiet: 0, fill: core::ptr::null_mut() })
    }

    pub(crate) fn bytes(&self) -> usize {
        bytes(self.span, self.acc.size())
    }

    /// # Safety — `base` writable.
    pub(crate) unsafe fn open(&self) {
        core::ptr::write_bytes(self.base, 0, HDR + 4 * self.span as usize);
    }

    #[inline(always)]
    unsafe fn hdr(&self) -> *mut u32 {
        self.base as *mut u32
    }

    #[inline(always)]
    unsafe fn fills(&self) -> *mut u64 {
        self.base.add(8) as *mut u64
    }

    /// Before a run under `split`: the leading records noted already are quiet, a fill lands in `fill` (writable through the run).
    pub(crate) unsafe fn stream(&mut self, split: SubSplit, fill: *mut u64) {
        self.quiet = Fills::quiet(self.fills(), split);
        self.fill = fill;
    }

    /// After the run (`Fills::advance`).
    pub(crate) unsafe fn advance(&self, split: SubSplit, nrecs: usize, v: i64, fill: u64) {
        Fills::advance(self.fills(), split, nrecs, v, fill)
    }

    /// The table's end (`Fills::reset`).
    pub(crate) unsafe fn end_stream(&self) {
        Fills::reset(self.fills())
    }

    #[inline(always)]
    pub(crate) fn quiet(&self) -> usize {
        self.quiet
    }

    #[inline(always)]
    pub(crate) unsafe fn filled(&self, i: usize) {
        if !self.fill.is_null() {
            *self.fill = i as u64;
        }
    }

    #[inline(always)]
    unsafe fn rec(&self, j: u32) -> *mut u8 {
        self.base.add(idx_bytes(self.span) + j as usize * self.acc.size() as usize)
    }

    pub(crate) unsafe fn len(&self) -> u32 {
        *self.hdr()
    }

    pub(crate) fn lo(&self) -> i64 {
        self.lo
    }

    pub(crate) fn span(&self) -> u64 {
        self.span as u64
    }

    /// D597: the fold met a key outside the domain (it kept the record out of the table); the reduce refuses by name.
    pub(crate) unsafe fn set_bad(&self) {
        *(self.base.add(BAD) as *mut u64) = 1;
    }

    pub(crate) unsafe fn bad(&self) -> bool {
        *(self.base.add(BAD) as *const u64) != 0
    }

    /// `new`: the identity table opened the pair. The group's accumulator opens on first sight.
    #[inline(always)]
    pub(crate) unsafe fn note(&self, ri: u64, src: *const u8, new: bool, f: &crate::table::owner_fold::Fold) {
        debug_assert!(ri < self.span as u64, "a group key outside its zone extrema");
        let slot = (self.base.add(HDR) as *mut u32).add(ri as usize);
        let mut j = *slot;
        if j == 0 {
            j = *self.hdr() + 1;
            *self.hdr() = j;
            *slot = j;
            let d = self.rec(j - 1);
            core::ptr::write_bytes(d, 0, self.acc.size() as usize);
            Rec::at(d, &self.acc).open(core::slice::from_raw_parts(src as *const u64, 1));
        }
        let d = self.rec(j - 1);
        if new {
            *(d.add(8) as *mut u64) += 1;
        }
        f.fold_first(d, src, self.nslots);
    }

    /// The round's end: every accumulator into `r2`'s buffer of its group-hash partition; `REDUCE_FLUSH` when the
    /// pool is exhausted (resumed after the flush), else 0. # Safety — `r2` the owner's round-2 table.
    pub(crate) unsafe fn append(&self, r2: *mut T1) -> i64 {
        let n = *self.hdr();
        let pbits = (*r2).pbits;
        let mut ap = T1::appender(r2);
        let mut i = *self.hdr().add(1);
        while i < n {
            let src = self.rec(i);
            let part = (KeyHash::of(core::slice::from_raw_parts(src as *const u64, 1))).part(pbits as u32);
            if !ap.append(part, src) {
                ap.finish();
                *self.hdr().add(1) = i;
                return REDUCE_FLUSH;
            }
            i += 1;
        }
        ap.finish();
        *self.hdr().add(1) = n;
        0
    }
}

/// exec's dense area seen from the identity fold (lower.md §6.12.8; `owner_fold::area_fold`): the count inside the fold
/// over a program-declared layout. The accumulator's form is fixed — one key word, the sentinel count
/// (`AREA_SENTINEL`), the pair count (`AREA_PAIRS`), then the pair's riding slots in the pair's order, so a
/// two-word pair's slot at byte `o` lands at `o + AREA_SHIFT`; exec checks its layouts against these before it
/// installs the hook. A key outside the domain raises `bad` (the reduce refuses by name) and is not inserted.
/// `quiet` leading records of a run were noted by a coarser read of the same stream (a table that filled: the
/// identity is inserted, nothing counted); `fill` takes the delivered index of the record met with the table full.
pub struct Area {
    groups: *mut u32,
    bad: *mut u32,
    slots: *mut u32,
    recs: *mut u8,
    acc: usize,
    lo: i64,
    span: u32,
    quiet: usize,
    fill: *mut u64,
}

pub const AREA_SENTINEL: usize = 1;
pub const AREA_PAIRS: usize = 2;
pub const AREA_SHIFT: usize = 8;

impl Area {
    /// # Safety — `groups` the opened-group counter, `bad` the flag word, `slots` the zero-initialised `span`
    /// index words (the group's 1-based accumulator), `recs` room for `span` accumulators of `acc` bytes,
    /// `fill` a writable word.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn over(groups: *mut u32, bad: *mut u32, slots: *mut u32, recs: *mut u8, acc: usize, lo: i64, span: u32, quiet: usize, fill: *mut u64) -> Area {
        Area { groups, bad, slots, recs, acc, lo, span, quiet, fill }
    }

    /// The accumulator of group index `ri` (its key `lo + ri`), opened on first sight.
    #[inline(always)]
    pub(crate) unsafe fn group(&self, ri: u64) -> *mut u64 {
        let slot = self.slots.add(ri as usize);
        let mut j = *slot;
        if j == 0 {
            j = *self.groups + 1;
            *self.groups = j;
            *slot = j;
            let d = self.recs.add((j - 1) as usize * self.acc) as *mut u64;
            core::ptr::write_bytes(d as *mut u8, 0, self.acc);
            *d = self.lo.wrapping_add(ri as i64) as u64;
            *d.add(AREA_SENTINEL) = 1;
        }
        self.recs.add((j - 1) as usize * self.acc) as *mut u64
    }

    #[inline(always)]
    pub(crate) fn lo(&self) -> i64 {
        self.lo
    }

    #[inline(always)]
    pub(crate) fn span(&self) -> u64 {
        self.span as u64
    }

    #[inline(always)]
    pub(crate) fn quiet(&self) -> usize {
        self.quiet
    }

    #[inline(always)]
    pub(crate) unsafe fn bad(&self) {
        *self.bad = 1;
    }

    #[inline(always)]
    pub(crate) unsafe fn filled(&self, i: usize) {
        *self.fill = i as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::sort::topn::t3::{misc_word, packed, shape_word_nn, K_INT, MODE_HASH, ORDER_COUNT_DESC, OUT_COUNT, OUT_DISTINCT, OUT_KEY, P_KEYS, P_LIMIT, P_MISC, P_OUT, P_SHAPE, P_SLOTS, S_AVG, S_COUNT, S_SUM};
    use crate::table::owner_fold::Fold;
    use gravity_abi::breaker::{KeyKind, SlotKind, HASHAGG_HDR, T1_HDR};
    use std::collections::BTreeMap;

    fn lcg(s: &mut u64) -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s >> 17
    }

    /// The plan words of a two-int-key group-by with `count(*)`, `sum`, `avg`, NOT NULL, the domain word.
    fn plan(lo: i64, hi: i64, slots: bool) -> [u64; NPARAMS] {
        let mut p = [0u64; NPARAMS];
        p[P_LIMIT] = 10;
        let ns = if slots { 3 } else { 0 };
        p[P_SHAPE] = shape_word_nn(2, ns, ORDER_COUNT_DESC, MODE_HASH, 0, None, 0, 2 + ns, true);
        p[P_KEYS] = packed(&[(0, K_INT), (1, K_INT)], 4);
        p[P_SLOTS] = if slots { packed(&[(0, S_COUNT), (2, S_SUM), (3, S_AVG)], 4) } else { 0 };
        p[P_OUT] = packed(&[(0, OUT_KEY), (0, OUT_DISTINCT), (0, OUT_COUNT)], 4);
        p[P_OUTVAL] = word(lo, hi);
        p[P_MISC] = misc_word(4, 0, 9_040);
        p
    }

    /// `n` distinct `(region, user)` pairs over `span` regions from `lo` in a `cap`-slot table (slot values from the
    /// pair's hash), and per region: pairs, count(*), sum, avg sum / n.
    fn pairs(lo: i64, span: u32, n: usize, cap: u32, seed: u64, pair: &RecordLayout) -> (Vec<u64>, BTreeMap<i64, (i64, i64, i64, i128, i64)>) {
        let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * pair.size() as usize) / 8 + 1]; // alloc: Cold
        let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, pair, cap, (region.len() * 8) as u32) };
        let mut rng = seed;
        let mut model = BTreeMap::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut slot = 0usize;
        while seen.len() < n {
            let r = lo + (lcg(&mut rng) % span as u64) as i64;
            let u = lcg(&mut rng) % (n as u64 * 4);
            if !seen.insert((r, u)) {
                continue;
            }
            slot += 1 + (lcg(&mut rng) % 2) as usize;
            let rec = t.record(slot % cap as usize);
            if rec.used() {
                seen.remove(&(r, u));
                continue;
            }
            rec.open(&[r as u64, u]);
            let (c, s, a) = ((u % 7) as i64 + 1, (u % 1000) as i64 - 500, (u % 91) as i64);
            let m = model.entry(r).or_insert((0, 0, 0, 0i128, 0));
            m.0 += 1;
            if pair.nslots() > 1 {
                rec.add_count(0, c);
                rec.add_i64(1, s);
                rec.add_avg(2, a as i128, c);
                m.1 += c;
                m.2 += s;
                m.3 += a as i128;
                m.4 += c;
            }
        }
        (region, model)
    }

    #[test]
    fn domain_word_round_trips() {
        for &(lo, hi) in &[(0i64, 0i64), (0, 249_999), (-5, 5), (1 << 30, (1 << 30) + 12_207), (-(1 << 39), -(1 << 39) + 3)] {
            let w = word(lo, hi);
            assert_ne!(w, 0, "{lo}..{hi}");
            assert_eq!(domain(w), Some((lo, (hi - lo + 1) as u32)), "{lo}..{hi}");
        }
        assert_eq!(domain(0), None);
        assert_eq!(word(5, 4), 0);
        assert_eq!(word(0, SPAN_MAX as i64), 0);
        assert_ne!(word(0, SPAN_MAX as i64 - 1), 0);
        // the widest span round-trips through the 23 span bits
        assert_eq!(domain(word(-5, SPAN_MAX as i64 - 6)), Some((-5, SPAN_MAX as u32)));
        assert_eq!(word(1 << 39, (1 << 39) + 1), 0);
        assert_eq!(word(i64::MIN, i64::MIN + 1), 0);
    }

    #[test]
    fn the_hook_counts_pairs_and_folds_slots_then_appends() {
        let (lo, span, n) = (1_000i64, 977u32, 3_000usize);
        let p = plan(lo, lo + span as i64 - 1, true);
        let pl = Plan::from_words(&p).unwrap();
        let (acc, pair) = (pl.layout_of(1, 1), pl.layout());
        assert_eq!(acc.nslots(), 4);
        let (mut region, model) = pairs(lo, span, n, 8_192, 7, &pair);
        let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &pair, 8_192, (region.len() * 8) as u32) };
        let mut space = vec![0u64; (bytes(span, acc.size()) + MARKER + FLOOR) / 8 + 1]; // alloc: Cold
        let start = space.as_mut_ptr() as *mut u8;
        let d = unsafe { Dense::of(p.as_ptr(), start, space.len() * 8) }.expect("the plan takes the dense index");
        assert!(unsafe { Dense::of(p.as_ptr(), start, d.bytes() + MARKER + FLOOR - 8) }.is_none());
        assert!(unsafe { Dense::of(plan(lo, lo + span as i64 - 1, false).as_ptr(), start, space.len() * 8) }.is_some());
        unsafe { d.open() };
        let f = Fold::dense(&pair).expect("the pair layout takes the dense hook");
        // every table record noted once as new, then every record again as a repeat (its slots fold twice, its pair once)
        for pass in 0..2 {
            for i in 0..t.cap as usize {
                let r = t.record(i);
                if r.used() {
                    unsafe { d.note((r.key(0) as i64 - lo) as u64, r.as_ptr(), pass == 0, &f) };
                }
            }
        }
        assert_eq!(unsafe { d.len() } as usize, model.len());
        let mut got = BTreeMap::new();
        for j in 0..unsafe { d.len() } {
            let r = unsafe { Rec::at(d.rec(j), &acc) };
            let (s, an) = r.avg(3);
            assert!(got.insert(r.key(0) as i64, (r.count(0), r.count(1) / 2, r.i64(2) / 2, s / 2, an / 2)).is_none());
        }
        assert_eq!(got, model);
        let mut small = vec![0u64; 640]; // alloc: Cold
        let r2 = unsafe { T1::at(small.as_mut_ptr() as *mut u8, &acc, 3, 4_096, (small.len() * 8) as u32) as *mut T1 };
        assert_eq!(unsafe { d.append(r2) }, REDUCE_FLUSH);
        let done1 = unsafe { *d.hdr().add(1) };
        assert!(done1 > 0 && (done1 as usize) < model.len());
        assert_eq!(unsafe { (*r2).buffered }, done1 as u64);
        let mut large = vec![0u64; 1 << 16]; // alloc: Cold
        let r2 = unsafe { T1::at(large.as_mut_ptr() as *mut u8, &acc, 3, 4_096, (large.len() * 8) as u32) as *mut T1 };
        assert_eq!(unsafe { d.append(r2) }, 0);
        assert_eq!(unsafe { (*r2).buffered } as usize, model.len() - done1 as usize);
        assert!(T1_HDR < 640 * 8);
    }

    #[test]
    fn the_marker_before_the_table_says_whether_the_area_is_live() {
        let (lo, span) = (10i64, 5u32);
        let p = plan(lo, lo + span as i64 - 1, false);
        let pair = Plan::from_words(&p).unwrap().layout();
        let acc = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let b = bytes(span, acc.size());
        // room for the area, its marker and the floor: the area is laid and opened, the table after the marker sees it
        let mut region = vec![0u64; (b + MARKER + FLOOR + 64) / 8]; // alloc: Cold
        let start = region.as_mut_ptr() as *mut u8;
        let took = unsafe { Dense::lay(p.as_ptr(), start, region.len() * 8) };
        assert_eq!(took, b + MARKER);
        let limit = region.len() * 8 - took;
        let t = unsafe { HashAgg::at(start.add(took), &pair, 64, limit as u32) as *const HashAgg };
        let d = unsafe { Dense::at_table(p.as_ptr(), t) }.expect("the marker names the area");
        assert_eq!(d.base, start);
        assert_eq!(unsafe { d.len() }, 0);
        // the floor short by a word: the marker alone, zero, and the table is the pair mode's
        let mut region = vec![0u64; (b + MARKER + FLOOR - 8) / 8]; // alloc: Cold
        let start = region.as_mut_ptr() as *mut u8;
        assert_eq!(unsafe { Dense::lay(p.as_ptr(), start, region.len() * 8) }, MARKER);
        let t = unsafe { HashAgg::at(start.add(MARKER), &pair, 64, (region.len() * 8 - MARKER) as u32) as *const HashAgg };
        assert!(unsafe { Dense::at_table(p.as_ptr(), t) }.is_none());
        // a plan without the form takes nothing
        let mut q = p;
        q[P_OUTVAL] = 0;
        assert_eq!(unsafe { Dense::lay(q.as_ptr(), start, region.len() * 8) }, 0);
    }

    #[test]
    fn the_fills_keep_the_noted_prefix_quiet_across_the_re_read_and_the_split() {
        let mut w = [0u64; Fills::WORDS];
        let at = |depth: u8, reread: u8| SubSplit { pbits: 3, depth, reread, sub: 0 };
        // depth 0: two runs, the third fills at its 7th record → fill[0] = 27, the stream rewound
        unsafe {
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 0)), 0);
            Fills::advance(w.as_mut_ptr(), at(0, 0), 10, 10, 0);
            Fills::advance(w.as_mut_ptr(), at(0, 0), 10, 10, 0);
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 0)), 0);
            Fills::advance(w.as_mut_ptr(), at(0, 0), 10, -1 - 6, 7);
            assert_eq!(w[..2], [0, 27]);
            // the re-read at depth 0 keeps 27 quiet, run by run; a same-depth child without `reread` would not
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 1)), 27);
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 0)), 0);
            Fills::advance(w.as_mut_ptr(), at(0, 1), 10, 10, 0);
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 1)), 17);
            Fills::advance(w.as_mut_ptr(), at(0, 1), 10, 10, 0);
            Fills::advance(w.as_mut_ptr(), at(0, 1), 10, 10, 0);
            assert_eq!(Fills::quiet(w.as_ptr(), at(0, 1)), 0);
            // the re-read fills at 35: the children at depth 1 keep 35 quiet, a depth-1 fill before it changes nothing
            Fills::advance(w.as_mut_ptr(), at(0, 1), 10, -1 - 4, 5);
            assert_eq!(w[..3], [0, 35, 0]);
            assert_eq!(Fills::quiet(w.as_ptr(), at(1, 0)), 35);
            Fills::advance(w.as_mut_ptr(), at(1, 0), 10, -1 - 2, 3);
            assert_eq!(w[..3], [0, 35, 3]);
            assert_eq!(Fills::quiet(w.as_ptr(), at(2, 0)), 35);
            Fills::reset(w.as_mut_ptr());
            assert_eq!(Fills::quiet(w.as_ptr(), at(1, 0)), 35);
        }
    }

    #[test]
    fn area_bytes() {
        assert_eq!((HDR, BAD), (16 + 8 * Fills::WORDS, 8 + 8 * Fills::WORDS));
        assert_eq!(idx_bytes(1), HDR + 8);
        assert_eq!(idx_bytes(3), HDR + 16);
        assert_eq!(bytes(250_000, 24), HDR + 1_000_000 + 6_000_000);
    }
}
