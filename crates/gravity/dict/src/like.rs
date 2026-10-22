//! LIKE / ILIKE classified once per query: any, exact, prefix, suffix, contains (the rare-byte-pair
//! SIMD scan, `substr`), segments (literal runs between `%`s, each found by its own scan), or
//! PostgreSQL's general matcher (`like_match.c`, the UTF-8 stamp) — bytes
//! under a deterministic collation; ILIKE folds ASCII (ctype C), a locale fold is the caller's shim
//! over lowered entries. A contains pattern over a part whose entries lie in one payload is one
//! scan of the payload (`build_bits_over`), never a per-entry call.
use crate::substr::{count_bytes, RarePair, Step};
use crate::{Bits, BuildError, Dict, Fault, Region, View};
use gravity_abi::hash::hash_bytes64;

/// `LIKE pattern must not end with escape character`: raised by PostgreSQL only when the match
/// reaches the escape, so it is a per-entry fault, never a classification error.
pub const FAULT_TRAILING_ESCAPE: Fault = Fault(1);
const ESCAPE: u8 = b'\\';
const LIKE_TRUE: i8 = 1;
const LIKE_FALSE: i8 = 0;
const LIKE_ABORT: i8 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Like,
    ILike,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Any,
    Exact,
    Prefix,
    Suffix,
    Contains,
    /// D3288: two or more literal runs between `%`s (LIKE only, no `_`): each run found leftmost in
    /// order, a leading run as a prefix and a trailing run as a suffix past the runs before it.
    Segments,
    General,
}

/// One literal run of a `Segments` pattern and its scan.
#[derive(Clone, Copy, Debug)]
struct Seg<'p> {
    lit: &'p [u8],
    pair: Option<RarePair>,
}

impl Seg<'_> {
    /// The run's first occurrence in `s` at or after `from`.
    #[inline]
    fn find(&self, s: &[u8], from: usize) -> Option<usize> {
        match self.pair {
            Some(p) => p.find(self.lit, s, from),
            None => memchr::memchr(self.lit[0], s.get(from..)?).map(|o| o + from),
        }
    }
}

pub struct Matcher<'p> {
    pattern: &'p [u8],
    lit: &'p [u8],
    class: Class,
    fold: bool,
    negate: bool,
    pair: Option<RarePair>,
    recount: Option<fn(u64, u64, u64, u64) -> bool>,
    segs: &'p [Seg<'p>],
    /// `Segments`: the first run starts the text (no leading `%`), the last ends it (no trailing `%`).
    anchored: (bool, bool),
}

#[inline(always)]
fn continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

struct Shape {
    lead: bool,
    trail: bool,
    lit_from: usize,
    lit_to: usize,
    lit_len: usize,
    escapes: bool,
    general: bool,
    all_percent: bool,
}

/// One pass over the pattern: `%`-runs at the ends, one literal run between, escapes counted;
/// `_`, an interior `%` or a trailing escape make it general.
fn shape(p: &[u8]) -> Shape {
    let mut s = Shape { lead: false, trail: false, lit_from: 0, lit_to: 0, lit_len: 0, escapes: false, general: false, all_percent: true };
    let mut i = 0;
    let mut seen_lit = false;
    let mut in_lit = false;
    while i < p.len() {
        let b = p[i];
        if b == b'%' {
            if seen_lit {
                s.trail = true;
                in_lit = false;
            } else {
                s.lead = true;
            }
            i += 1;
            continue;
        }
        s.all_percent = false;
        if b == b'_' || s.trail {
            s.general = true;
            return s;
        }
        if !in_lit {
            in_lit = true;
            seen_lit = true;
            s.lit_from = i;
        }
        if b == ESCAPE {
            s.escapes = true;
            if i + 1 >= p.len() {
                s.general = true;
                return s;
            }
            i += 2;
        } else {
            i += 1;
        }
        s.lit_len += 1;
        s.lit_to = i;
    }
    s
}

/// D3288: `p`'s literal runs when it is two or more of them between `%`s with no `_` and no trailing
/// escape, each run starting on a character's first byte (else `None`: the general matcher), the
/// runs and their scans laid in `region` (a full region is the caller's retry, as for a literal);
/// and whether the first run starts the pattern and the last one ends it.
#[allow(clippy::type_complexity)]
fn segments<'p>(p: &'p [u8], region: &mut Region) -> Result<Option<(&'p [Seg<'p>], (bool, bool))>, BuildError> {
    let (mut n, mut i, mut in_run, mut last_pct, mut escapes) = (0usize, 0usize, false, false, false);
    while i < p.len() {
        let b = p[i];
        if b == b'%' {
            (in_run, last_pct) = (false, true);
            i += 1;
            continue;
        }
        if b == b'_' || (b == ESCAPE && i + 1 >= p.len()) {
            return Ok(None);
        }
        let lit = if b == ESCAPE { p[i + 1] } else { b };
        if !in_run {
            if continuation(lit) {
                return Ok(None);
            }
            (n, in_run) = (n + 1, true);
        }
        escapes |= b == ESCAPE;
        last_pct = false;
        i += if b == ESCAPE { 2 } else { 1 };
    }
    if n < 2 {
        return Ok(None);
    }
    let bytes = if escapes { region.alloc(p.len(), 1)? } else { core::ptr::null_mut() };
    let segs = region.alloc(n * core::mem::size_of::<Seg<'p>>(), core::mem::align_of::<Seg<'p>>())? as *mut Seg<'p>;
    let (mut k, mut i, mut used) = (0usize, 0usize, 0usize);
    while i < p.len() {
        if p[i] == b'%' {
            i += 1;
            continue;
        }
        let start = i;
        let mut esc = false;
        while i < p.len() && p[i] != b'%' {
            esc |= p[i] == ESCAPE;
            i += if p[i] == ESCAPE { 2 } else { 1 };
        }
        let lit: &'p [u8] = if esc {
            let from = used;
            let mut j = start;
            while j < i {
                let c = if p[j] == ESCAPE { j += 1; p[j] } else { p[j] };
                // SAFETY: `bytes` holds `p.len()` bytes and the unescaped runs take at most that many.
                unsafe { *bytes.add(used) = c };
                (used, j) = (used + 1, j + 1);
            }
            // SAFETY: `from..used` was written above, in the region, which outlives the matcher.
            unsafe { core::slice::from_raw_parts(bytes.add(from), used - from) }
        } else {
            &p[start..i]
        };
        // SAFETY: `segs` holds `n` runs; `k < n` (the first pass counted them).
        unsafe { segs.add(k).write(Seg { lit, pair: RarePair::new(lit) }) };
        k += 1;
    }
    // SAFETY: the `n` runs were written above, in the region, which outlives the matcher.
    Ok(Some((unsafe { core::slice::from_raw_parts(segs, n) }, (p[0] != b'%', !last_pct))))
}

impl<'p> Matcher<'p> {
    /// The pattern is in PostgreSQL's backslash convention (`like_escape` applied by the caller);
    /// the literal is borrowed from it or unescaped / folded into `region`, whose memory outlives
    /// the matcher (the region stays usable: the table is built into it next).
    pub fn new(pattern: &'p [u8], mode: Mode, region: &mut Region) -> Result<Matcher<'p>, BuildError> {
        let fold = mode == Mode::ILike;
        let sh = shape(pattern);
        let class = if sh.general {
            Class::General
        } else if sh.all_percent && !pattern.is_empty() {
            Class::Any
        } else {
            let first = pattern.get(sh.lit_from).copied().unwrap_or(0);
            match (sh.lead, sh.trail) {
                (false, false) => Class::Exact,
                (false, true) => Class::Prefix,
                (true, _) if continuation(first) || (first == ESCAPE && pattern.get(sh.lit_from + 1).is_some_and(|&b| continuation(b))) => Class::General,
                (true, false) => Class::Suffix,
                (true, true) => Class::Contains,
            }
        };
        let raw = if sh.general { &[][..] } else { &pattern[sh.lit_from..sh.lit_to] };
        let needs_copy = sh.escapes || (fold && raw.iter().any(u8::is_ascii_uppercase));
        let lit: &'p [u8] = if matches!(class, Class::Any | Class::General) {
            &[]
        } else if needs_copy {
            let dst = region.alloc(sh.lit_len, 1)?;
            let mut n = 0;
            let mut i = 0;
            while i < raw.len() {
                let b = if raw[i] == ESCAPE {
                    i += 1;
                    raw[i]
                } else {
                    raw[i]
                };
                // SAFETY: `dst` holds `lit_len` bytes, one per literal character.
                unsafe { *dst.add(n) = if fold { b.to_ascii_lowercase() } else { b } };
                n += 1;
                i += 1;
            }
            // SAFETY: `n == lit_len` bytes were written into the region.
            unsafe { core::slice::from_raw_parts(dst, n) }
        } else {
            raw
        };
        let pair = (class == Class::Contains && !fold).then(|| RarePair::new(lit)).flatten();
        let mut m = Matcher { pattern, lit, class, fold, negate: false, pair, recount: None, segs: &[], anchored: (false, false) };
        if class == Class::General && !fold {
            if let Some((segs, anchored)) = segments(pattern, region)? {
                (m.class, m.segs, m.anchored) = (Class::Segments, segs, anchored);
            }
        }
        Ok(m)
    }

    /// The re-check rule of `build_bits_over`'s contains scan (dict worklist item 18, B2-01, mechanism R): `rule(bytes
    /// scanned since the last count, candidates rejected among them, bytes counted before, bytes left)`, asked at a
    /// block that wasted a candidate, says when counting the new bytes pays (the host's cost home,
    /// `cost_profile::scan::Scan::recount_pays`); the scan then re-ranks its rare byte pair from the counts of all the
    /// payload it has counted. Without a rule the static pair scans the whole payload.
    pub fn recounting(mut self, rule: fn(u64, u64, u64, u64) -> bool) -> Matcher<'p> {
        self.recount = Some(rule);
        self
    }

    /// `NOT LIKE` / `NOT ILIKE`: the verdict inverted, a fault kept.
    pub fn negated(mut self) -> Matcher<'p> {
        self.negate = !self.negate;
        self
    }

    pub fn class(&self) -> Class {
        self.class
    }

    pub fn literal(&self) -> &[u8] {
        self.lit
    }

    /// The expression's identity for the once-cell key: pattern bytes, mode and negation.
    pub fn fingerprint(&self) -> u64 {
        hash_bytes64(self.pattern) ^ ((self.fold as u64) << 62) ^ ((self.negate as u64) << 63)
    }

    #[inline]
    fn eq(&self, a: &[u8], b: &[u8]) -> bool {
        if self.fold {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }

    #[inline]
    pub fn matches(&self, s: &[u8]) -> Result<bool, Fault> {
        let lit = self.lit;
        let hit = match self.class {
            Class::Any => true,
            Class::Exact => self.eq(s, lit),
            Class::Prefix => s.len() >= lit.len() && self.eq(&s[..lit.len()], lit),
            Class::Suffix => s.len() >= lit.len() && self.eq(&s[s.len() - lit.len()..], lit),
            Class::Contains if self.fold => contains_fold(s, lit),
            Class::Contains => self.find(s, 0).is_some(),
            Class::Segments => self.segments_hit(s),
            Class::General => {
                let r = if self.fold { match_text::<true>(s, self.pattern)? } else { match_text::<false>(s, self.pattern)? };
                r == LIKE_TRUE
            }
        };
        Ok(hit != self.negate)
    }

    /// `Segments`: leftmost-first decides `like_match.c` when `%` is the only wildcard — the earliest
    /// end of each run leaves the most text for the runs after it — and a byte search is a
    /// character search for a run that starts on a character's first byte (`segments` takes no other).
    #[inline]
    fn segments_hit(&self, s: &[u8]) -> bool {
        let (head, tail) = self.anchored;
        let mut runs = self.segs;
        let mut at = 0usize;
        if head {
            let first = runs[0].lit;
            if !s.starts_with(first) {
                return false;
            }
            (at, runs) = (first.len(), &runs[1..]);
        }
        let last = if tail { runs.split_last().map(|(l, rest)| (runs = rest, l.lit).1) } else { None };
        for g in runs {
            match g.find(s, at) {
                Some(p) => at = p + g.lit.len(),
                None => return false,
            }
        }
        last.is_none_or(|l| s.len() >= at + l.len() && s.ends_with(l))
    }

    /// The first occurrence of the contains literal in `s` at or after `from` (bytes, no fold).
    #[inline]
    fn find(&self, s: &[u8], from: usize) -> Option<usize> {
        match (&self.pair, self.lit.first()) {
            (Some(p), _) => p.find(self.lit, s, from),
            (None, Some(&b)) => memchr::memchr(b, &s[from.min(s.len())..]).map(|o| o + from),
            (None, None) => Some(from),
        }
    }

    /// The predicate over every entry as a `Bits` table. A contains pattern under a `recounting`
    /// rule carries `build_bits_over`'s re-check across the entries (`build_bits_rechecking`).
    pub fn build_bits(&self, dict: &dyn Dict, region: &mut Region) -> Result<Bits, BuildError> {
        if let (Class::Contains, false, Some(pair), Some(rule)) = (self.class, self.fold, self.pair, self.recount) {
            return self.build_bits_rechecking(dict, region, pair, rule);
        }
        // SAFETY: the dictionary's views are live through the sweep.
        Bits::build(dict, region, &mut |v| self.matches(unsafe { v.bytes() }))
    }

    /// The per-entry contains build with the re-check of `build_bits_over` (B2-01): rejected candidates
    /// (`RarePair::find_counting`) and the bytes read are carried from entry to entry, the rule asked after an entry that
    /// wasted one, the bytes left estimated from the mean entry so far. A count the rule asks for is taken forward, over
    /// as many bytes of the entries that follow as were read since the last count (entries are not revisited), and the
    /// pair is re-ranked from all the counts once it is complete; a re-rank that keeps the pair ends the re-checks.
    fn build_bits_rechecking(&self, dict: &dyn Dict, region: &mut Region, start: RarePair, rule: fn(u64, u64, u64, u64) -> bool) -> Result<Bits, BuildError> {
        let (lit, negate, n) = (self.lit, self.negate, dict.ncodes() as u64);
        let mut pair = start;
        let mut counts = [0u64; 256];
        let (mut counted, mut since, mut rejected, mut pending, mut done, mut read) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        let mut settled = false;
        Bits::build(dict, region, &mut |v| {
            // SAFETY: the dictionary's views are live through the sweep.
            let s = unsafe { v.bytes() };
            done += 1;
            read += s.len() as u64;
            if settled {
                return Ok(pair.find(lit, s, 0).is_some() != negate);
            }
            if pending == 0 {
                let before = rejected;
                let hit = pair.find_counting(lit, s, &mut rejected).is_some();
                since += s.len() as u64;
                // the rule is asked after an entry that wasted a candidate; the bytes left come from the mean entry so far
                if rejected == before || !rule(since, rejected, counted, n.saturating_sub(done).saturating_mul(read / done)) {
                    return Ok(hit != negate);
                }
                (pending, since, rejected) = (since, 0, 0);
                return Ok(hit != negate);
            }
            let k = (pending as usize).min(s.len());
            count_bytes(&s[..k], &mut counts);
            (pending, counted) = (pending - k as u64, counted + k as u64);
            let hit = pair.find(lit, s, 0).is_some();
            if pending == 0 {
                // a re-rank that keeps the pair ends the re-checks: the counts agree with it
                let next = RarePair::sampled(lit, &counts).unwrap_or(pair);
                (settled, pair) = (next == pair, next);
            }
            Ok(hit != negate)
        })
    }

    /// The predicate over a part whose entries are spans of one `payload` (`index`): a contains
    /// pattern is ONE scan of the payload — each hit resolved to its entry by a binary search on
    /// the ascending spans, a hit outside every entry's bytes rejected, the scan resumed after the
    /// hit (a rejected one) or the entry (an accepted one), so entries are never revisited and a
    /// hit straddling two entries hides nothing; the other classes compare each span in place. The
    /// pair starts as the static rank's and, under a `recounting` rule, is re-ranked from the
    /// payload's own byte counts when the waste its candidates project pays for counting.
    pub fn build_bits_over(&self, index: &dyn EntryIndex, payload: &[u8], region: &mut Region) -> Result<Bits, BuildError> {
        let dict = PayloadDict { index, payload };
        if !self.streams() || !dict.ascending() {
            return self.build_bits(&dict, region);
        }
        Bits::of_hits(&dict, self.negate, region, &mut |set| self.scan_entries(index, payload, set))
    }

    /// Whether `scan_entries` takes this pattern: a contains literal (no fold) or segments.
    pub fn streams(&self) -> bool {
        match self.class {
            Class::Contains => !self.fold && !self.lit.is_empty(),
            Class::Segments => true,
            _ => false,
        }
    }

    pub fn is_negated(&self) -> bool {
        self.negate
    }

    /// The entries of `payload` (`index`'s ascending spans) the pattern matches, each handed to `hit` once (the
    /// verdict before any negation): ONE scan of the payload for the contains literal, or for a Segments pattern's first
    /// run, each hit resolved to its entry by a binary search on the spans, a hit outside every entry's bytes (or in
    /// the NULL code's) rejected and the scan resumed after it; a contains hit takes the entry, a Segments hit decides
    /// the whole entry by its runs from its start (D3288), and either way the scan resumes after the entry. Only a
    /// pattern that `streams`.
    pub fn scan_entries(&self, index: &dyn EntryIndex, payload: &[u8], hit: &mut dyn FnMut(u32)) {
        let (scan, first) = match self.class {
            Class::Segments => (self.segs[0].lit, self.segs[0].pair),
            _ => (self.lit, self.pair),
        };
        let mut from = 0usize;
        let mut pair = first;
        let mut counts = [0u64; 256];
        let (mut counted, mut rejected) = (0usize, 0u64);
        loop {
            let h = match (pair, self.recount) {
                (Some(p), Some(rule)) => match p.find_rechecking(scan, payload, from, &mut rejected, |at, r| rule(at.saturating_sub(counted) as u64, r, counted as u64, payload.len().saturating_sub(at) as u64)) {
                    Step::Hit(h) => h,
                    Step::Recheck(at) => {
                        count_bytes(&payload[counted..at], &mut counts);
                        (counted, rejected, from) = (at, 0, at);
                        pair = RarePair::sampled(scan, &counts).or(pair);
                        continue;
                    }
                    Step::End => break,
                },
                _ => match (Seg { lit: scan, pair }).find(payload, from) {
                    Some(h) => h,
                    None => break,
                },
            };
            let c = index.code_at(h);
            let (s, l) = index.span(c);
            if h >= s && h + scan.len() <= s + l && Some(c) != index.null_code() {
                if self.class == Class::Contains || self.segments_hit(&payload[s..(s + l).min(payload.len())]) {
                    hit(c);
                }
                from = s + l;
            } else {
                from = h + 1;
            }
        }
    }
}

/// The entries of a part as byte spans `(start, len)` of one payload, ascending in code order.
pub trait EntryIndex {
    fn ncodes(&self) -> u32;
    fn span(&self, code: u32) -> (usize, usize);
    fn null_code(&self) -> Option<u32> {
        None
    }
    /// The last code whose span starts at or before `pos` (ascending spans).
    fn code_at(&self, pos: usize) -> u32 {
        let (mut lo, mut hi) = (0u32, self.ncodes());
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if self.span(mid).0 <= pos {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// An `EntryIndex` over its payload as the library's `Dict`.
pub struct PayloadDict<'a> {
    pub index: &'a dyn EntryIndex,
    pub payload: &'a [u8],
}

impl PayloadDict<'_> {
    fn ascending(&self) -> bool {
        let mut end = 0usize;
        for c in 0..self.index.ncodes() {
            let (s, l) = self.index.span(c);
            if s < end || s + l > self.payload.len() {
                return false;
            }
            end = s + l;
        }
        true
    }
}

impl Dict for PayloadDict<'_> {
    fn ncodes(&self) -> u32 {
        self.index.ncodes()
    }

    fn null_code(&self) -> Option<u32> {
        self.index.null_code()
    }

    fn entries(&self, f: &mut dyn FnMut(u32, View)) {
        for c in 0..self.index.ncodes() {
            let (s, l) = self.index.span(c);
            f(c, View::of(&self.payload[s.min(self.payload.len())..(s + l).min(self.payload.len())]));
        }
    }
}

/// ASCII-folded `memmem` over a folded literal: candidates by the first byte's two cases.
fn contains_fold(s: &[u8], lit: &[u8]) -> bool {
    let Some(&f) = lit.first() else { return true };
    let (lo, up) = (f, f.to_ascii_uppercase());
    let mut from = 0;
    while from + lit.len() <= s.len() {
        let hay = &s[from..s.len() - lit.len() + 1];
        let Some(off) = (if lo == up { memchr::memchr(lo, hay) } else { memchr::memchr2(lo, up, hay) }) else { return false };
        let at = from + off;
        if s[at + 1..at + lit.len()].eq_ignore_ascii_case(&lit[1..]) {
            return true;
        }
        from = at + 1;
    }
    false
}

#[inline(always)]
fn next_char(mut t: &[u8]) -> &[u8] {
    loop {
        t = &t[1..];
        if t.is_empty() || !continuation(t[0]) {
            return t;
        }
    }
}

/// The next text position whose byte is `firstpat` under the fold: `like_match.c` steps by
/// characters and compares the byte, and every non-continuation byte is a character start, so
/// a `memchr` over the bytes visits exactly the positions the step would (`firstpat` is one).
#[inline(always)]
fn scan<const F: bool>(t: &[u8], firstpat: u8) -> Option<usize> {
    let up = firstpat.to_ascii_uppercase();
    if F && up != firstpat {
        memchr::memchr2(firstpat, up, t)
    } else {
        memchr::memchr(firstpat, t)
    }
}

#[inline(always)]
fn ch<const F: bool>(b: u8) -> u8 {
    if F {
        b.to_ascii_lowercase()
    } else {
        b
    }
}

/// `like_match.c` `MatchText`, the UTF-8 stamp: `%` and `_` step by characters, everything else
/// by bytes; `F` folds ASCII (the lowered-operand ILIKE path). D4009: without recursion.
/// MatchText recurses at each `%`, and a FALSE from a deeper level only ever resumes the
/// innermost `%`'s scan (a `%` level answers TRUE, ABORT or a fault, never FALSE), so one saved
/// scan point stands for the whole recursion: the same verdicts, the same lazily raised
/// trailing-escape fault, O(n * m) steps and no stack. The recursive port ran a pattern of
/// 200000 `%a` groups off the backend thread's stack (SIGSEGV: the server); PG raises 54001
/// there (check_stack_depth), which answering instead falls under 'resource limits quiet'.
fn match_text<const F: bool>(t: &[u8], p: &[u8]) -> Result<i8, Fault> {
    if p.len() == 1 && p[0] == b'%' {
        return Ok(LIKE_TRUE);
    }
    let (mut t, mut p) = (t, p);
    let mut back: Option<Back<'_>> = None;
    loop {
        let r = level::<F>(&mut t, &mut p, &mut back)?;
        if r != LIKE_FALSE {
            return Ok(r);
        }
        // a level's FALSE returns to the innermost `%`, which tries its next position
        let Some(b) = back else { return Ok(LIKE_FALSE) };
        if b.single {
            return Ok(LIKE_ABORT);
        }
        let rest = next_char(b.at);
        if rest.is_empty() {
            return Ok(LIKE_ABORT);
        }
        let Some(at) = scan::<F>(rest, b.firstpat) else { return Ok(LIKE_ABORT) };
        (t, p) = (&rest[at..], b.pat);
        back = Some(Back { at: t, ..b });
    }
}

/// The innermost `%` of [`match_text`]: the text position it tried last, the pattern at its first
/// literal, that literal (folded), and whether MatchText tries that one position only (a
/// continuation byte).
#[derive(Clone, Copy)]
struct Back<'a> {
    at: &'a [u8],
    pat: &'a [u8],
    firstpat: u8,
    single: bool,
}

/// One MatchText level from `(t, p)`: its verdict, or, at a `%`, the scan's first try continued in
/// place (`back` holds that `%`), as MatchText's recursive call would run it.
#[inline(always)]
fn level<'a, const F: bool>(t: &mut &'a [u8], p: &mut &'a [u8], back: &mut Option<Back<'a>>) -> Result<i8, Fault> {
    while !t.is_empty() && !p.is_empty() {
        if p[0] == b'%' {
            *p = &p[1..];
            while !p.is_empty() {
                if p[0] == b'%' {
                    *p = &p[1..];
                } else if p[0] == b'_' {
                    if t.is_empty() {
                        return Ok(LIKE_ABORT);
                    }
                    *t = next_char(t);
                    *p = &p[1..];
                } else {
                    break;
                }
            }
            if p.is_empty() {
                return Ok(LIKE_TRUE);
            }
            let firstpat = if p[0] == ESCAPE {
                if p.len() < 2 {
                    return Err(FAULT_TRAILING_ESCAPE);
                }
                ch::<F>(p[1])
            } else {
                ch::<F>(p[0])
            };
            if continuation(firstpat) {
                if t.is_empty() || t[0] != firstpat {
                    return Ok(LIKE_ABORT);
                }
                *back = Some(Back { at: t, pat: p, firstpat, single: true });
                continue;
            }
            if t.is_empty() {
                return Ok(LIKE_ABORT);
            }
            let Some(at) = scan::<F>(t, firstpat) else { return Ok(LIKE_ABORT) };
            *t = &t[at..];
            *back = Some(Back { at: t, pat: p, firstpat, single: false });
            continue;
        } else if p[0] == b'_' {
            *t = next_char(t);
            *p = &p[1..];
            continue;
        } else if p[0] == ESCAPE {
            *p = &p[1..];
            if p.is_empty() {
                return Err(FAULT_TRAILING_ESCAPE);
            }
            if ch::<F>(p[0]) != ch::<F>(t[0]) {
                return Ok(LIKE_FALSE);
            }
        } else if ch::<F>(p[0]) != ch::<F>(t[0]) {
            return Ok(LIKE_FALSE);
        }
        *t = &t[1..];
        *p = &p[1..];
    }
    if !t.is_empty() {
        return Ok(LIKE_FALSE);
    }
    while !p.is_empty() && p[0] == b'%' {
        *p = &p[1..];
    }
    if p.is_empty() {
        return Ok(LIKE_TRUE);
    }
    Ok(LIKE_ABORT)
}

#[cfg(test)]
mod d4009 {
    //! D4009: the iterative `match_text` against the recursive port it replaced (MatchText's own
    //! control flow): the same verdict (TRUE / FALSE / ABORT) and the same trailing-escape fault on
    //! a seeded random corpus, both folds; and a depth the recursive port ran off the stack.
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// The recursive port (trunk before D4009), the oracle.
    fn recursive<const F: bool>(mut t: &[u8], mut p: &[u8]) -> Result<i8, Fault> {
        if p.len() == 1 && p[0] == b'%' {
            return Ok(LIKE_TRUE);
        }
        while !t.is_empty() && !p.is_empty() {
            if p[0] == b'%' {
                p = &p[1..];
                while !p.is_empty() {
                    if p[0] == b'%' {
                        p = &p[1..];
                    } else if p[0] == b'_' {
                        if t.is_empty() {
                            return Ok(LIKE_ABORT);
                        }
                        t = next_char(t);
                        p = &p[1..];
                    } else {
                        break;
                    }
                }
                if p.is_empty() {
                    return Ok(LIKE_TRUE);
                }
                let firstpat = if p[0] == ESCAPE {
                    if p.len() < 2 {
                        return Err(FAULT_TRAILING_ESCAPE);
                    }
                    ch::<F>(p[1])
                } else {
                    ch::<F>(p[0])
                };
                if continuation(firstpat) {
                    if !t.is_empty() && t[0] == firstpat {
                        let m = recursive::<F>(t, p)?;
                        if m != LIKE_FALSE {
                            return Ok(m);
                        }
                    }
                    return Ok(LIKE_ABORT);
                }
                while !t.is_empty() {
                    let Some(at) = scan::<F>(t, firstpat) else { return Ok(LIKE_ABORT) };
                    t = &t[at..];
                    let m = recursive::<F>(t, p)?;
                    if m != LIKE_FALSE {
                        return Ok(m);
                    }
                    t = next_char(t);
                }
                return Ok(LIKE_ABORT);
            } else if p[0] == b'_' {
                t = next_char(t);
                p = &p[1..];
                continue;
            } else if p[0] == ESCAPE {
                p = &p[1..];
                if p.is_empty() {
                    return Err(FAULT_TRAILING_ESCAPE);
                }
                if ch::<F>(p[0]) != ch::<F>(t[0]) {
                    return Ok(LIKE_FALSE);
                }
            } else if ch::<F>(p[0]) != ch::<F>(t[0]) {
                return Ok(LIKE_FALSE);
            }
            t = &t[1..];
            p = &p[1..];
        }
        if !t.is_empty() {
            return Ok(LIKE_FALSE);
        }
        while !p.is_empty() && p[0] == b'%' {
            p = &p[1..];
        }
        if p.is_empty() {
            return Ok(LIKE_TRUE);
        }
        Ok(LIKE_ABORT)
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    fn word(r: &mut Rng, alphabet: &[&str], max: u64) -> Vec<u8> {
        let n = r.next() % (max + 1);
        (0..n).flat_map(|_| r.pick(alphabet).bytes()).collect()
    }

    #[test]
    fn the_iterative_matcher_answers_as_the_recursive_port() {
        let pat = ["%", "%", "_", "\\", "a", "b", "A", "\u{e9}", "\u{65e5}", "%a", "a_"];
        let txt = ["a", "b", "A", "B", "\u{e9}", "\u{c9}", "\u{65e5}", "%", "_", "\\"];
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut n, mut faults, mut trues) = (0usize, 0usize, 0usize);
        for _ in 0..200_000 {
            let (p, t) = (word(&mut r, &pat, 9), word(&mut r, &txt, 12));
            for fold in [false, true] {
                let (got, want) = if fold { (match_text::<true>(&t, &p), recursive::<true>(&t, &p)) } else { (match_text::<false>(&t, &p), recursive::<false>(&t, &p)) };
                assert_eq!(got, want, "{:?} LIKE {:?} fold {fold}", core::str::from_utf8(&t), core::str::from_utf8(&p));
                faults += got.is_err() as usize;
                trues += (got == Ok(LIKE_TRUE)) as usize;
                n += 1;
            }
        }
        assert!(faults > 100 && trues > 10_000 && n == 400_000, "the corpus reaches every verdict: {faults} faults, {trues} matches of {n}");
    }

    #[test]
    fn a_pattern_of_two_hundred_thousand_groups_needs_no_stack() {
        let text: Vec<u8> = core::iter::repeat_n(b'a', 400_000).collect();
        let deep: Vec<u8> = core::iter::repeat_n(&b"%a"[..], 200_000).flatten().copied().chain([b'%']).collect();
        assert_eq!(match_text::<true>(&text, &deep), Ok(LIKE_TRUE));
        assert_eq!(match_text::<false>(&text, &deep), Ok(LIKE_TRUE));
        let under: Vec<u8> = core::iter::repeat_n(&b"%a_"[..], 200_000).flatten().copied().chain([b'%']).collect();
        assert_eq!(match_text::<false>(&text, &under), Ok(LIKE_TRUE));
        assert_eq!(match_text::<false>(&text[..399_999], &under), Ok(LIKE_ABORT), "one 'a' short");
    }
}
