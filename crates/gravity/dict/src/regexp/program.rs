//! The pattern subset compiled once into an op program and run by a greedy leftmost-first backtracker. Two callers run
//! the same program: the per-row tier under RE2 (regexp_alt's `Re2Pattern::exec`) and the per-entry dictionary
//! evaluation (`super`).
//!
//! The subset: an optional leading `^`; literal runs (escapes decoded per the classifier's whitelist); optional literals
//! `X?` and `(?:literal)?`; ASCII-member bracket classes, plain or negated, and `.`, each under `* + ? {n[,m]}`; up to nine
//! capture groups, top level and unquantified, holding the above; `$` as the final atom. A class that admits non-ASCII
//! characters (a negated class, `.`) counts and gives back whole UTF-8 characters, as RE2's runes. No alternation, so the
//! greedy first-match order is RE2's Perl mode on valid UTF-8; anything else compiles to `None`.
//!
//! Case-insensitive (`Program::compile_folded`, the dictionary path's `(?i)` under a collation whose case map takes every
//! ASCII letter to its ASCII twin): the pattern's ASCII letters match either case, as Spencer's `allcases` expands each
//! literal letter and each bracket member under `REG_ICASE`; a non-ASCII byte in the pattern is refused (its cases are
//! the collation's), and a subject's non-ASCII character never matches an ASCII letter (Spencer: the Kelvin sign is not
//! `k`'s case).
//!
//! A match is the leftmost: an unanchored program tries each character boundary in turn (a leading literal found by
//! `memmem`, a leading class's non-members skipped, a failed possessive leading run skipped whole). The runner counts its
//! steps: a subject past the budget is refused (`Outcome::Refused`), never answered differently. Every subject is valid
//! UTF-8 (the per-row tier's data guard; the dictionary path serves UTF-8 databases only).

/// A match's span as the per-row tier keeps it: `(start, end)` byte offsets, `(-1, -1)` for a group that took no part.
pub type GroupSpan = (i64, i64);

pub use gravity_abi::limits::{REGEXP_MAX_GROUPS as MAX_GROUPS, REGEXP_MAX_LITS as MAX_LITS, REGEXP_MAX_OPS as MAX_OPS};

const UNSET: (usize, usize) = (usize::MAX, usize::MAX);

fn step_budget(len: usize) -> u64 {
    1024 + 8 * len as u64
}

#[derive(Clone, Copy)]
struct ByteSet([u64; 4]);

impl ByteSet {
    const fn empty() -> ByteSet {
        ByteSet([0; 4])
    }

    fn insert(&mut self, b: u8) {
        self.0[(b >> 6) as usize] |= 1u64 << (b & 63);
    }

    #[inline(always)]
    fn contains(&self, b: u8) -> bool {
        self.0[(b >> 6) as usize] & (1u64 << (b & 63)) != 0
    }

    fn negate(&mut self) {
        for w in &mut self.0 {
            *w = !*w;
        }
    }

    fn disjoint(&self, o: &ByteSet) -> bool {
        self.0.iter().zip(&o.0).all(|(a, b)| a & b == 0)
    }

    /// Every ASCII letter member with its other case (a bracket under `REG_ICASE`, before any negation).
    fn fold_ascii(&mut self) {
        for b in b'a'..=b'z' {
            if self.contains(b) || self.contains(b.to_ascii_uppercase()) {
                self.insert(b);
                self.insert(b.to_ascii_uppercase());
            }
        }
    }

    fn excluded_ascii(&self) -> ([u8; 4], usize) {
        let (mut out, mut n) = ([0u8; 4], 0usize);
        for b in 0u8..128 {
            if !self.contains(b) {
                if n < 4 {
                    out[n] = b;
                }
                n += 1;
            }
        }
        (out, n)
    }
}

#[derive(Clone, Copy)]
enum Scan {
    Not1(u8),
    Not2(u8, u8),
    Not3(u8, u8, u8),
    All,
    Table,
}

#[derive(Clone, Copy)]
enum Op {
    Lit { off: u16, len: u16 },
    Opt { off: u16, len: u16 },
    /// `chars`: the class admits non-ASCII characters, so its bounds count characters; `possessive`: only the greedy
    /// length can match (`mark_possessive`).
    Set { set: ByteSet, scan: Scan, min: u32, max: u32, chars: bool, possessive: bool },
    CapStart(u8),
    CapEnd(u8),
    /// A trailing `.*` / `.+`: every remaining byte, read or not.
    Tail { min_one: bool },
    End,
}

#[derive(Clone, Copy)]
enum Lead {
    None,
    Lit { off: u16, len: u16 },
    Set(ByteSet),
}

/// A compiled pattern: fixed storage, no allocation.
pub struct Program {
    ops: [Op; MAX_OPS],
    nops: usize,
    lits: [u8; MAX_LITS],
    nlits: usize,
    ngroups: usize,
    anchored: bool,
    /// ASCII letters match either case (`compile_folded`): `lits` holds them lowercase, compared folded; every class is
    /// closed over case.
    fold: bool,
    /// Every class run is possessive: the only choice points left are the optional literals, so `Run::run_linear` walks
    /// the ops in a loop with a stack of their untaken arms instead of recursing.
    linear: bool,
    lead: Lead,
}

/// A match: its span, its groups (`group`), and what decided it — every byte compared lies in `hay[..read]`, and
/// `saw_end` says the subject's length took part (`gravity_dict::regexp::Memo`'s contract, anchored programs).
#[derive(Clone, Copy)]
pub struct Found {
    pub start: usize,
    pub end: usize,
    caps: [(usize, usize); MAX_GROUPS],
    pub read: usize,
    pub saw_end: bool,
}

impl Found {
    /// Group `g` (1-based), `None` past the pattern's groups.
    pub fn group(&self, g: usize) -> Option<(usize, usize)> {
        (1..=MAX_GROUPS).contains(&g).then(|| self.caps[g - 1]).filter(|&c| c != UNSET)
    }
}

/// A match as the dictionary's in-place plan reads it (`Program::find_group`): its end, the one group the template
/// names, and the memo's `read` / `saw_end`; no copy of the other groups.
#[derive(Clone, Copy)]
pub struct Grouped {
    pub end: usize,
    /// The group's span, `(0, 0)` where it took no part (a replacement's empty `\n`); group 0 is the whole match.
    pub group: (usize, usize),
    pub read: usize,
    pub saw_end: bool,
}

pub enum Outcome<F = Found> {
    NoMatch,
    Match(F),
    /// The step budget ran out: the caller's engine answers.
    Refused,
}

pub(crate) fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[inline(always)]
fn continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

fn decode_escape(c: u8) -> Option<u8> {
    Some(match c {
        b'n' => b'\n',
        b't' => b'\t',
        b'r' => b'\r',
        b'f' => 0x0C,
        b'v' => 0x0B,
        b'a' => 0x07,
        _ if c.is_ascii() && !c.is_ascii_alphanumeric() => c,
        _ => return None,
    })
}

struct Compiler<'a> {
    pat: &'a [u8],
    i: usize,
    prog: Program,
    run_start: usize,
}

impl Compiler<'_> {
    fn peek(&self) -> Option<u8> {
        self.pat.get(self.i).copied()
    }

    fn push_lit_byte(&mut self, b: u8) -> Option<()> {
        if self.prog.nlits >= MAX_LITS {
            return None;
        }
        self.prog.lits[self.prog.nlits] = b;
        self.prog.nlits += 1;
        Some(())
    }

    fn flush_run(&mut self) -> Option<()> {
        if self.prog.nlits > self.run_start {
            let (off, len) = (self.run_start as u16, (self.prog.nlits - self.run_start) as u16);
            self.run_start = self.prog.nlits;
            self.push_op(Op::Lit { off, len })?;
        }
        Some(())
    }

    fn push_op(&mut self, op: Op) -> Option<()> {
        if self.prog.nops >= MAX_OPS {
            return None;
        }
        self.prog.ops[self.prog.nops] = op;
        self.prog.nops += 1;
        Some(())
    }

    fn push(&mut self, op: Op) -> Option<()> {
        self.flush_run()?;
        self.push_op(op)
    }

    /// One literal atom (a byte, a UTF-8 character or a decoded escape) appended to `lits`.
    fn literal_atom(&mut self) -> Option<usize> {
        let b = self.peek()?;
        match b {
            b'\\' => {
                let c = *self.pat.get(self.i + 1)?;
                self.push_lit_byte(decode_escape(c)?)?;
                self.i += 2;
                Some(1)
            }
            b'(' | b')' | b'[' | b']' | b'{' | b'*' | b'+' | b'?' | b'|' | b'^' | b'$' | b'.' => None,
            _ => {
                let len = utf8_char_len(b);
                if self.i + len > self.pat.len() || (self.prog.fold && !b.is_ascii()) {
                    return None;
                }
                if self.prog.fold {
                    self.push_lit_byte(b.to_ascii_lowercase())?;
                    self.i += 1;
                    return Some(1);
                }
                for k in 0..len {
                    self.push_lit_byte(self.pat[self.i + k])?;
                }
                self.i += len;
                Some(len)
            }
        }
    }

    fn class(&mut self) -> Option<(ByteSet, bool)> {
        self.i += 1;
        let negated = self.peek() == Some(b'^');
        if negated {
            self.i += 1;
        }
        let mut set = ByteSet::empty();
        let mut prev: Option<u8> = None;
        let mut any = false;
        loop {
            let b = self.peek()?;
            match b {
                b']' if any => {
                    self.i += 1;
                    break;
                }
                b']' | b'\\' | b'[' => return None,
                b'-' => {
                    if self.pat.get(self.i + 1) == Some(&b']') || (!any && prev.is_none()) {
                        set.insert(b'-');
                        self.i += 1;
                        any = true;
                        prev = None;
                    } else if let Some(lo) = prev {
                        let hi = *self.pat.get(self.i + 1)?;
                        if !hi.is_ascii() || matches!(hi, b'\\' | b'[' | b']') || hi < lo {
                            return None;
                        }
                        for x in lo..=hi {
                            set.insert(x);
                        }
                        self.i += 2;
                        any = true;
                        prev = None;
                    } else {
                        return None;
                    }
                }
                _ => {
                    if !b.is_ascii() {
                        return None;
                    }
                    set.insert(b);
                    self.i += 1;
                    any = true;
                    prev = Some(b);
                }
            }
        }
        if self.prog.fold {
            set.fold_ascii();
        }
        Some((set, negated))
    }

    fn number(&mut self) -> Option<u32> {
        let (start, mut m) = (self.i, 0u32);
        while let Some(d @ b'0'..=b'9') = self.peek() {
            m = m.checked_mul(10)?.checked_add((d - b'0') as u32)?;
            if m > 255 {
                return None;
            }
            self.i += 1;
        }
        (self.i > start).then_some(m)
    }

    /// `(min, max)` with `u32::MAX` unbounded; `(1, 1)` without a quantifier.
    fn quantifier(&mut self) -> Option<(u32, u32)> {
        let q = match self.peek() {
            Some(b'*') => (0, u32::MAX),
            Some(b'+') => (1, u32::MAX),
            Some(b'?') => (0, 1),
            Some(b'{') => {
                self.i += 1;
                let m = self.number()?;
                let q = match self.peek() {
                    Some(b'}') => (m, m),
                    Some(b',') => {
                        self.i += 1;
                        if self.peek() == Some(b'}') {
                            (m, u32::MAX)
                        } else {
                            let n = self.number()?;
                            if n < m || self.peek() != Some(b'}') {
                                return None;
                            }
                            (m, n)
                        }
                    }
                    _ => return None,
                };
                self.i += 1;
                return self.greedy(q);
            }
            _ => return Some((1, 1)),
        };
        self.i += 1;
        self.greedy(q)
    }

    /// A non-greedy quantifier (`*?`) or a doubled one is outside the subset.
    fn greedy(&self, q: (u32, u32)) -> Option<(u32, u32)> {
        (!matches!(self.peek(), Some(b'?' | b'*' | b'+' | b'{'))).then_some(q)
    }

    fn push_set(&mut self, set: ByteSet, negated: bool, min: u32, max: u32) -> Option<()> {
        let (set, chars) = if negated {
            let mut s = set;
            s.negate();
            (s, true)
        } else {
            (set, false)
        };
        let scan = if !chars {
            Scan::Table
        } else {
            match set.excluded_ascii() {
                (_, 0) => Scan::All,
                ([a, ..], 1) => Scan::Not1(a),
                ([a, b, ..], 2) => Scan::Not2(a, b),
                ([a, b, c, _], 3) => Scan::Not3(a, b, c),
                _ => Scan::Table,
            }
        };
        self.push(Op::Set { set, scan, min, max, chars, possessive: false })
    }

    fn quantified_atom(&mut self, atom_off: usize, atom_len: usize) -> Option<()> {
        match self.quantifier()? {
            (1, 1) => Some(()),
            (0, 1) => {
                let bytes_end = self.prog.nlits;
                self.prog.nlits = atom_off;
                self.flush_run()?;
                self.prog.nlits = bytes_end;
                self.run_start = bytes_end;
                self.push_op(Op::Opt { off: atom_off as u16, len: atom_len as u16 })
            }
            (min, max) => {
                if atom_len != 1 || !self.prog.lits[atom_off].is_ascii() {
                    return None;
                }
                let b = self.prog.lits[atom_off];
                self.prog.nlits = atom_off;
                self.flush_run()?;
                let mut set = ByteSet::empty();
                set.insert(b);
                if self.prog.fold {
                    set.fold_ascii();
                }
                self.push_set(set, false, min, max)
            }
        }
    }

    fn seq(&mut self, in_group: bool) -> Option<()> {
        while let Some(b) = self.peek() {
            match b {
                b')' if in_group => return Some(()),
                b')' | b'|' | b'^' => return None,
                b'$' => {
                    if in_group || self.i + 1 != self.pat.len() {
                        return None;
                    }
                    self.i += 1;
                    self.push(Op::End)?;
                }
                b'.' => {
                    let q = self.pat.get(self.i + 1).copied();
                    let rest = &self.pat[(self.i + 2).min(self.pat.len())..];
                    if !in_group && matches!(q, Some(b'*' | b'+')) && (rest.is_empty() || rest == b"$") {
                        self.i += 2;
                        self.push(Op::Tail { min_one: q == Some(b'+') })?;
                        continue;
                    }
                    self.i += 1;
                    let (min, max) = self.quantifier()?;
                    self.push_set(ByteSet::empty(), true, min, max)?;
                }
                b'(' => {
                    if self.pat.get(self.i + 1) == Some(&b'?') {
                        if self.pat.get(self.i + 2) != Some(&b':') {
                            return None;
                        }
                        self.i += 3;
                        let off = self.prog.nlits;
                        while self.peek() != Some(b')') {
                            self.literal_atom()?;
                        }
                        self.i += 1;
                        self.quantified_atom(off, self.prog.nlits - off)?;
                    } else {
                        if in_group || self.prog.ngroups == MAX_GROUPS {
                            return None;
                        }
                        let g = self.prog.ngroups as u8;
                        self.prog.ngroups += 1;
                        self.i += 1;
                        self.push(Op::CapStart(g))?;
                        self.seq(true)?;
                        if self.peek() != Some(b')') {
                            return None;
                        }
                        self.i += 1;
                        self.push(Op::CapEnd(g))?;
                        if matches!(self.peek(), Some(b'*' | b'+' | b'?' | b'{')) {
                            return None;
                        }
                    }
                }
                b'[' => {
                    let (set, negated) = self.class()?;
                    let (min, max) = self.quantifier()?;
                    self.push_set(set, negated, min, max)?;
                }
                _ => {
                    let off = self.prog.nlits;
                    let len = self.literal_atom()?;
                    self.quantified_atom(off, len)?;
                }
            }
        }
        (!in_group).then_some(())
    }
}

impl Program {
    /// `None`: outside the subset (the caller keeps its engine).
    pub fn compile(pattern: &[u8]) -> Option<Program> {
        Self::build(pattern, false)
    }

    /// `pattern` under `REG_ICASE` (an ARE's `(?i)` prefix, stripped by the caller), for a collation whose regex case map
    /// (regc_pg_locale.c `pg_wc_toupper` / `pg_wc_tolower`) takes every ASCII letter to its ASCII twin; `None` outside
    /// the subset or for a non-ASCII byte in the pattern.
    pub fn compile_folded(pattern: &[u8]) -> Option<Program> {
        Self::build(pattern, true)
    }

    fn build(pattern: &[u8], fold: bool) -> Option<Program> {
        let prog = Program { ops: [Op::End; MAX_OPS], nops: 0, lits: [0; MAX_LITS], nlits: 0, ngroups: 0, anchored: false, fold, linear: false, lead: Lead::None };
        let mut c = Compiler { pat: pattern, i: 0, prog, run_start: 0 };
        if c.peek() == Some(b'^') {
            c.prog.anchored = true;
            c.i += 1;
        }
        if pattern.starts_with(b"***") {
            return None;
        }
        c.seq(false)?;
        c.flush_run()?;
        let mut prog = c.prog;
        prog.mark_possessive();
        prog.lead = match prog.consuming(0).map(|i| prog.ops[i]) {
            // folded, a literal with a letter: the first byte's cases (`memmem` compares exactly)
            Some(Op::Lit { off, len }) if len > 0 && fold && prog.lit(off, len).iter().any(u8::is_ascii_alphabetic) => {
                let mut set = ByteSet::empty();
                set.insert(prog.lits[off as usize]);
                set.fold_ascii();
                Lead::Set(set)
            }
            Some(Op::Lit { off, len }) if len > 0 => Lead::Lit { off, len },
            Some(Op::Set { set, min, .. }) if min >= 1 => Lead::Set(set),
            _ => Lead::None,
        };
        Some(prog)
    }

    /// The first op at or past `from` that consumes (the group marks consume nothing).
    fn consuming(&self, from: usize) -> Option<usize> {
        (from..self.nops).find(|&i| !matches!(self.ops[i], Op::CapStart(_) | Op::CapEnd(_)))
    }

    /// D2778: PostgreSQL's ARE takes the longest match at a start (a greedy first quantified atom), this runner the first
    /// in greedy order. They agree when every match ends at the subject's end, or when no choice point (an optional
    /// literal the next byte does not decide, a class run that can give back) has a variable-length op after it:
    /// `^(?:ab)?(?:abcd)?` over `abcd` is PostgreSQL's `abcd`, the greedy order's `ab`. An optional literal whose next
    /// consuming op is a literal of another first byte (`s?` in `^https?://`), or a class of one or more that excludes
    /// its first byte (`v?` in `/v?[0-9]+`), is decided by the subject's byte, no choice. RE2's Perl mode (the per-row
    /// tier) is the greedy order itself: only the dictionary path asks.
    pub fn longest_is_first(&self) -> bool {
        let ops = &self.ops[..self.nops];
        let decided = |i: usize, off: u16| match self.consuming(i + 1).map(|k| ops[k]) {
            Some(Op::Lit { off: o, len }) => len > 0 && self.lits[o as usize] != self.lits[off as usize],
            Some(Op::Set { set, min, .. }) => min >= 1 && !set.contains(self.lits[off as usize]),
            _ => false,
        };
        let choice = |(i, op): (usize, &Op)| match op {
            Op::Opt { off, len } => *len > 0 && !decided(i, *off),
            Op::Set { possessive: false, min, max, .. } => min != max,
            _ => false,
        };
        let variable = |op: &Op| matches!(op, Op::Opt { .. } | Op::Tail { .. }) || matches!(op, Op::Set { min, max, .. } if min != max);
        self.anchored_to_end() || ops.iter().enumerate().position(choice).is_none_or(|i| !ops[i + 1..].iter().any(variable))
    }

    /// A class run whose next consuming op is `$`, the program's end, a literal whose first byte the class excludes, or a
    /// class of one or more members the class excludes, is possessive: a shorter length puts that next op on a member of
    /// this class and fails (or, at the end, the greedy length already matched). The backtracking loop then only re-proves
    /// the failure one position at a time (a `[^/]+/` over a slash-less subject, pinned by gravity_dict's
    /// tests/regexp_linear.rs). The skipped attempts compare only bytes the scan already read, so a memo's `read` /
    /// `saw_end` stay exact. `$` is the subject's end alone, never before a '\n' (no newline-sensitive flag in the subset).
    fn mark_possessive(&mut self) {
        for i in 0..self.nops {
            let Op::Set { set, .. } = self.ops[i] else { continue };
            let p = match self.consuming(i + 1).map(|k| self.ops[k]) {
                None | Some(Op::End) | Some(Op::Tail { min_one: false }) => true,
                Some(Op::Lit { off, len }) => len > 0 && !set.contains(self.lits[off as usize]),
                Some(Op::Set { set: next, min, .. }) => min >= 1 && set.disjoint(&next),
                _ => false,
            };
            if let Op::Set { possessive, .. } = &mut self.ops[i] {
                *possessive = p;
            }
        }
        self.linear = self.ops[..self.nops].iter().all(|op| !matches!(op, Op::Set { possessive: false, .. }));
    }

    pub fn ngroups(&self) -> usize {
        self.ngroups
    }

    /// `^`: a match starts at the subject's start only.
    pub fn anchored(&self) -> bool {
        self.anchored
    }

    /// `run_linear` serves this program (tests).
    pub fn linear(&self) -> bool {
        self.linear
    }

    /// Every match holds a byte at its start (the lead: a literal, or a class of one or more): none starts at the end.
    pub fn needs_byte(&self) -> bool {
        !matches!(self.lead, Lead::None)
    }

    /// Every match ends at the subject's end (a trailing `.*` or `$`).
    pub fn anchored_to_end(&self) -> bool {
        matches!(self.ops[..self.nops].last(), Some(Op::Tail { .. } | Op::End))
    }

    /// The leftmost match starting at or past `from` (a character boundary).
    pub fn find(&self, hay: &[u8], from: usize) -> Outcome {
        self.search(hay, from, self.linear, |r: &Run<'_>, s| r.found(s))
    }

    /// Tests: `find` forced through the recursive runner, the reference `run_linear` must equal.
    pub fn find_recursive(&self, hay: &[u8], from: usize) -> Outcome {
        self.search(hay, from, false, |r: &Run<'_>, s| r.found(s))
    }

    /// `find` from the subject's start reading group `g` alone (0: the whole match): the in-place plan's one call per
    /// dictionary entry, with no `Found` (every group's span) built, copied and returned per entry.
    #[inline(always)]
    pub fn find_group(&self, hay: &[u8], g: usize) -> Outcome<Grouped> {
        self.search(hay, 0, self.linear, |r: &Run<'_>, start| {
            let group = match g {
                0 => (start, r.end),
                _ if g <= MAX_GROUPS && r.caps[g - 1] != UNSET => r.caps[g - 1],
                _ => (0, 0),
            };
            Grouped { end: r.end, group, read: r.read, saw_end: r.saw_end }
        })
    }

    /// The per-row tier (`Re2Pattern::exec`): `out` filled as `Re2Re::match_at` fills it, `None` where RE2 answers (a
    /// refusal, an anchored program asked past the start).
    pub fn exec(&self, hay: &[u8], start: usize, out: &mut [GroupSpan]) -> Option<bool> {
        if self.anchored && start != 0 {
            return None;
        }
        match self.find(hay, start) {
            Outcome::Refused => None,
            Outcome::NoMatch => Some(false),
            Outcome::Match(f) => {
                let n = out.len().min(self.ngroups + 1);
                for (g, slot) in out.iter_mut().enumerate() {
                    *slot = match g {
                        0 if n > 0 => (f.start as i64, f.end as i64),
                        _ if g < n => f.group(g).map_or((-1, -1), |(s, e)| (s as i64, e as i64)),
                        _ => (-1, -1),
                    };
                }
                Some(true)
            }
        }
    }

    /// The leftmost match at or past `from`, `take` reading it off the run.
    #[inline(always)]
    #[inline(always)]
    fn search<F>(&self, hay: &[u8], from: usize, linear: bool, take: impl Fn(&Run<'_>, usize) -> F) -> Outcome<F> {
        match self.fold {
            true => self.search_in::<true, F>(hay, from, linear, take),
            false => self.search_in::<false, F>(hay, from, linear, take),
        }
    }

    /// `search` with the case rule a constant (`FOLD` = `self.fold`): the runner an unfolded program takes carries no
    /// folded comparison (D4953: a per-literal test of the rule cost every unfolded program's runner).
    #[inline(always)]
    fn search_in<const FOLD: bool, F>(&self, hay: &[u8], from: usize, linear: bool, take: impl Fn(&Run<'_>, usize) -> F) -> Outcome<F> {
        let mut r = Run { hay, prog: self, caps: [UNSET; MAX_GROUPS], end: 0, read: 0, saw_end: false, steps: step_budget(hay.len()), refused: false };
        if self.anchored {
            let ok = from == 0 && r.attempt::<FOLD>(0, linear);
            return r.outcome(ok, 0, take);
        }
        let mut s = from;
        loop {
            s = match self.lead {
                Lead::Lit { off, len } => match memchr::memmem::find(&hay[s..], self.lit(off, len)) {
                    Some(k) => s + k,
                    None => return Outcome::NoMatch,
                },
                Lead::Set(set) => match hay[s..].iter().position(|&b| set.contains(b)) {
                    Some(k) => s + k,
                    None => return Outcome::NoMatch,
                },
                Lead::None => s,
            };
            let ok = r.attempt::<FOLD>(s, linear);
            if ok || r.refused {
                r.read = hay.len();
                r.saw_end = true;
                return r.outcome(ok, s, take);
            }
            if s >= hay.len() {
                return Outcome::NoMatch;
            }
            s += r.skip(s);
        }
    }

    #[inline(always)]
    fn lit(&self, off: u16, len: u16) -> &[u8] {
        &self.lits[off as usize..off as usize + len as usize]
    }
}

struct Run<'a> {
    hay: &'a [u8],
    prog: &'a Program,
    caps: [(usize, usize); MAX_GROUPS],
    end: usize,
    read: usize,
    saw_end: bool,
    steps: u64,
    refused: bool,
}

impl Run<'_> {
    fn attempt<const FOLD: bool>(&mut self, s: usize, linear: bool) -> bool {
        if linear {
            self.run_linear::<FOLD>(s)
        } else {
            self.run::<FOLD>(0, s)
        }
    }

    #[inline(always)]
    fn outcome<F>(&self, ok: bool, start: usize, take: impl Fn(&Run<'_>, usize) -> F) -> Outcome<F> {
        match (ok, self.refused) {
            (_, true) => Outcome::Refused,
            (false, _) => Outcome::NoMatch,
            (true, _) => Outcome::Match(take(self, start)),
        }
    }

    fn found(&self, start: usize) -> Found {
        Found { start, end: self.end, caps: self.caps, read: self.read, saw_end: self.saw_end }
    }

    /// The next start after a failed attempt at `s`: past a possessive unbounded leading run (every start inside it ends
    /// the run at the same byte and fails the same way), else one character on.
    fn skip(&mut self, s: usize) -> usize {
        let one = utf8_char_len(self.hay[s]);
        let Some(i) = self.prog.consuming(0) else { return one };
        match self.prog.ops[i] {
            Op::Set { possessive: true, max: u32::MAX, .. } => self.span(i, s).map_or(one, |(_, e)| (e - s).max(one)),
            _ => one,
        }
    }

    #[inline(always)]
    fn step(&mut self) -> bool {
        if self.steps == 0 {
            self.refused = true;
            return false;
        }
        self.steps -= 1;
        true
    }

    /// Class op `ip`'s run at `pos`: `(floor, greedy end)` — the end after its minimum and after its maximum characters —
    /// or `None` short of the minimum. The bytes the scan read are recorded.
    #[inline(always)]
    fn span(&mut self, ip: usize, pos: usize) -> Option<(usize, usize)> {
        let Op::Set { set, scan, min, max, chars, .. } = self.prog.ops[ip] else { unreachable!("a class op") };
        let (hay, len) = (self.hay, self.hay.len());
        let rest = &hay[pos..];
        if !chars || (max == u32::MAX && min <= 1) {
            let cap = if max == u32::MAX { rest.len() } else { rest.len().min(max as usize) };
            let k = match scan {
                Scan::Not1(a) => memchr::memchr(a, &rest[..cap]).unwrap_or(cap),
                Scan::Not2(a, b) => memchr::memchr2(a, b, &rest[..cap]).unwrap_or(cap),
                Scan::Not3(a, b, c) => memchr::memchr3(a, b, c, &rest[..cap]).unwrap_or(cap),
                Scan::All => cap,
                Scan::Table => rest[..cap].iter().position(|&b| !set.contains(b)).unwrap_or(cap),
            };
            if k == cap && cap == rest.len() {
                self.saw_end = true;
            }
            self.read = self.read.max(match scan {
                Scan::All => len,
                _ => pos + (k + 1).min(rest.len()),
            });
            let floor = match (min, chars) {
                (0, _) => pos,
                (1, true) if k > 0 => pos + utf8_char_len(rest[0]),
                (m, _) => pos + m as usize,
            };
            return (k >= min as usize && floor <= pos + k).then_some((floor, pos + k));
        }
        let (mut e, mut c, mut floor) = (pos, 0u32, pos);
        while c < max && e < len && set.contains(hay[e]) {
            e += utf8_char_len(hay[e]);
            c += 1;
            if c == min {
                floor = e;
            }
        }
        let e = e.min(len);
        if c < max && e == len {
            self.saw_end = true;
        }
        self.read = self.read.max((e + 1).min(len));
        (c >= min).then_some((floor, e))
    }

    /// `run(0, s)` for a `linear` program: the same ops in the same order, the same reads (so `read`, `saw_end`, the
    /// groups and the end equal `run`'s), each optional literal's untaken arm stacked where `run` would recurse, and a
    /// failure resuming the latest one where `run` would return to it. A possessive class run tries its greedy length
    /// alone in both.
    fn run_linear<const FOLD: bool>(&mut self, s: usize) -> bool {
        let (prog, len) = (self.prog, self.hay.len());
        let mut alts = [(0u32, 0u32); MAX_OPS];
        let (mut n, mut ip, mut pos) = (0usize, 0usize, s);
        loop {
            if !self.step() {
                return false;
            }
            if ip >= prog.nops {
                self.end = pos;
                return true;
            }
            let ok = match prog.ops[ip] {
                Op::Lit { off, len: l } => {
                    let ok = self.lit_at::<FOLD>(pos, off, l);
                    pos += if ok { l as usize } else { 0 };
                    ok
                }
                Op::Opt { off, len: l } => {
                    if self.lit_at::<FOLD>(pos, off, l) {
                        alts[n] = ((ip + 1) as u32, pos as u32);
                        n += 1;
                        pos += l as usize;
                    }
                    true
                }
                Op::Set { .. } => match self.span(ip, pos) {
                    Some((_, e)) => {
                        pos = e;
                        true
                    }
                    None => false,
                },
                Op::CapStart(g) => {
                    self.caps[g as usize].0 = pos;
                    true
                }
                Op::CapEnd(g) => {
                    self.caps[g as usize].1 = pos;
                    true
                }
                Op::Tail { min_one } => {
                    self.saw_end |= min_one;
                    let ok = !(min_one && pos == len);
                    pos = if ok { len } else { pos };
                    ok
                }
                Op::End => {
                    if !(ip > 0 && matches!(prog.ops[ip - 1], Op::Tail { .. })) {
                        self.saw_end = true;
                    }
                    pos == len
                }
            };
            if ok {
                ip += 1;
            } else if n == 0 {
                return false;
            } else {
                n -= 1;
                (ip, pos) = (alts[n].0 as usize, alts[n].1 as usize);
            }
        }
    }

    fn run<const FOLD: bool>(&mut self, ip: usize, pos: usize) -> bool {
        if !self.step() {
            return false;
        }
        if ip >= self.prog.nops {
            self.end = pos;
            return true;
        }
        match self.prog.ops[ip] {
            Op::Lit { off, len } => self.lit_at::<FOLD>(pos, off, len) && self.run::<FOLD>(ip + 1, pos + len as usize),
            Op::Opt { off, len } => (self.lit_at::<FOLD>(pos, off, len) && self.run::<FOLD>(ip + 1, pos + len as usize)) || (!self.refused && self.run::<FOLD>(ip + 1, pos)),
            Op::Set { chars, possessive, .. } => {
                let Some((floor, mut e)) = self.span(ip, pos) else { return false };
                loop {
                    if self.run::<FOLD>(ip + 1, e) {
                        return true;
                    }
                    if self.refused || possessive || e <= floor {
                        return false;
                    }
                    e -= 1;
                    while chars && e > floor && continuation(self.hay[e]) {
                        e -= 1;
                    }
                }
            }
            Op::CapStart(g) => {
                self.caps[g as usize].0 = pos;
                self.run::<FOLD>(ip + 1, pos)
            }
            Op::CapEnd(g) => {
                self.caps[g as usize].1 = pos;
                self.run::<FOLD>(ip + 1, pos)
            }
            Op::Tail { min_one } => {
                self.saw_end |= min_one;
                !(min_one && pos == self.hay.len()) && self.run::<FOLD>(ip + 1, self.hay.len())
            }
            Op::End => {
                // `$` right after a tail read nothing new: the tail already stands at the end
                if !(ip > 0 && matches!(self.prog.ops[ip - 1], Op::Tail { .. })) {
                    self.saw_end = true;
                }
                pos == self.hay.len() && self.run::<FOLD>(ip + 1, pos)
            }
        }
    }

    /// The literal compared at `pos`, the comparison's reach recorded.
    #[inline(always)]
    fn lit_at<const FOLD: bool>(&mut self, pos: usize, off: u16, len: u16) -> bool {
        let rest = &self.hay[pos..];
        if rest.len() < len as usize {
            self.saw_end = true;
            self.read = self.hay.len();
            return false;
        }
        self.read = self.read.max(pos + len as usize);
        let lit = self.prog.lit(off, len);
        if FOLD {
            rest[..lit.len()].eq_ignore_ascii_case(lit)
        } else {
            rest.starts_with(lit)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn prog(p: &str) -> Program {
        Program::compile(p.as_bytes()).unwrap_or_else(|| panic!("should compile {p:?}"))
    }

    /// `(whole, groups)` of the leftmost match.
    fn m(p: &str, s: &str) -> Option<((usize, usize), Vec<Option<(usize, usize)>>)> {
        let pr = prog(p);
        let f = |o: Outcome| match o {
            Outcome::Match(f) => Some(((f.start, f.end), (1..=pr.ngroups()).map(|g| f.group(g)).collect::<Vec<_>>())),
            Outcome::NoMatch => None,
            Outcome::Refused => panic!("budget refusal on {p:?} / {s:?}"),
        };
        let (a, b) = (f(pr.find(s.as_bytes(), 0)), f(pr.find_recursive(s.as_bytes(), 0)));
        assert_eq!(a, b, "the linear and the recursive runner on {p:?} / {s:?}");
        a
    }

    /// A-16b: a folded program's ASCII letters match either case, in literals (the leading one found by its first byte's
    /// cases), optional literals, classes and their negations; a non-ASCII subject character never matches an ASCII
    /// letter (the Kelvin sign is not `k`); a non-ASCII pattern byte is refused.
    #[test]
    fn folded_letters_match_either_case() {
        let f = |p: &str, s: &str| match Program::compile_folded(p.as_bytes()).unwrap_or_else(|| panic!("should compile {p:?}")).find(s.as_bytes(), 0) {
            Outcome::Match(f) => Some((f.start, f.end, f.group(1))),
            Outcome::NoMatch => None,
            Outcome::Refused => panic!("budget refusal on {p:?} / {s:?}"),
        };
        assert_eq!(f("http://([a-z.]+)/", "x HTTP://Ab.C/d"), Some((2, 14, Some((9, 13)))));
        assert_eq!(f("^https?://(?:M\\.)?([^/]+)/", "Https://m.A/"), Some((0, 12, Some((10, 11)))));
        assert_eq!(f("[^a-c]+", "ABCdx"), Some((3, 5, None)));
        assert_eq!(f("k", "\u{212a}K"), Some((3, 4, None)));
        assert_eq!(f("k", "\u{212a}"), None);
        assert_eq!(f("x?y{2}", "XYy"), Some((0, 3, None)));
        assert!(Program::compile_folded("é".as_bytes()).is_none() && Program::compile_folded("[a-z]é".as_bytes()).is_none());
        assert!(Program::compile("é".as_bytes()).is_some(), "unfolded, a non-ASCII literal is bytes");
    }

    #[test]
    fn url_host_shape() {
        let p = r"^https?://(?:m\.)?([^/]+)/.*$";
        assert_eq!(m(p, "http://m.example.com/path/x?y=1"), Some(((0, 31), vec![Some((9, 20))]))); // alloc: Cold
        assert_eq!(m(p, "https://sub.host.ru/"), Some(((0, 20), vec![Some((8, 19))]))); // alloc: Cold
        assert_eq!(m(p, "not-a-url"), None);
        assert_eq!(m(p, "http://hostonly.com"), None);
        assert_eq!(m(p, ""), None);
        assert_eq!(m(p, "http://m/"), Some(((0, 9), vec![Some((7, 8))]))); // alloc: Cold
        let s = "https://пример.рф/страница";
        assert_eq!(m(p, s).unwrap().1, vec![Some((8, s.find("/страница").unwrap()))]); // alloc: Cold
    }

    #[test]
    fn subset_membership() {
        for p in [
            r"^https?://(?:m\.)?([^/]+)/.*$",
            "^abc",
            "abc",
            "^abc$",
            "^",
            "",
            "^$",
            "^a?b",
            "^(?:foo)?bar",
            "^[a-z]+$",
            "^[^,]*x",
            "^a{2,3}b",
            "^x*y+z?",
            "^([0-9]{4})-",
            "^a.*",
            "^a.+$",
            "^a.b",
            "^.*a",
            "a.{2,3}b",
            "^(a)(b)",
            "([a-z]+)/(.+)",
            "^(a)(b)(c)(d)(e)(f)(g)(h)(i)",
            "^[^/]",
            "^[^/]{2,3}",
            r"^\.\*x",
            "^é?x",
            "://([^/]+)/",
        ] {
            assert!(Program::compile(p.as_bytes()).is_some(), "should compile {p:?}");
        }
        for p in [
            "^a|b",
            "^(a)?",
            "^((a))",
            "^(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)",
            "^[é]+",
            "^é+",
            "^a$b",
            "^a^b",
            "^(?:a[b])?",
            "^(a$)",
            r"^\d",
            "^a*?b",
            "^a+*",
            "***:abc",
        ] {
            assert!(Program::compile(p.as_bytes()).is_none(), "should refuse {p:?}");
        }
    }

    #[test]
    fn greedy_give_back() {
        assert_eq!(m("^([a-z]+)z$", "abcz").unwrap().1, vec![Some((0, 3))]); // alloc: Cold
        assert_eq!(m("^([a-z]+)bc$", "abc").unwrap().1, vec![Some((0, 1))]); // alloc: Cold
        assert!(m("^[a-z]+z$", "z").is_none());
        assert!(m("^a{2,3}$", "a").is_none());
        assert!(m("^a{2,3}$", "aa").is_some());
        assert!(m("^a{2,3}$", "aaa").is_some());
        assert!(m("^a{2,3}$", "aaaa").is_none());
        assert!(m("^ab?bc$", "abc").is_some());
        assert_eq!(m(r"^(?:www\.)?([^/]+)$", "www.x").unwrap().1, vec![Some((4, 5))]); // alloc: Cold
        assert_eq!(m(r"^(?:www\.)?([^/]+)$", "www").unwrap().1, vec![Some((0, 3))]); // alloc: Cold
        assert_eq!(m("^(.*)/(.*)$", "a/b/c").unwrap().1, vec![Some((0, 3)), Some((4, 5))]); // alloc: Cold
        assert_eq!(m("^(.+)/[0-9]+/", "x/y/12/z").unwrap().1, vec![Some((0, 3))]); // alloc: Cold
    }

    #[test]
    fn characters_not_bytes() {
        assert_eq!(m("^([^a]+)y$", "xéy").unwrap().1, vec![Some((0, 3))]); // alloc: Cold
        assert_eq!(m("^([^a]+)[^b]+X$", "xéX").unwrap().1, vec![Some((0, 1))]); // alloc: Cold
        assert_eq!(m("^(.{2})", "éüx").unwrap().1, vec![Some((0, 4))]); // alloc: Cold
        assert_eq!(m("^(.)(.)$", "日本").unwrap().1, vec![Some((0, 3)), Some((3, 6))]); // alloc: Cold
        assert!(m("^.{3}$", "日本").is_none());
        assert_eq!(m("^([^/]{2,3})", "ab/").unwrap().1, vec![Some((0, 2))]); // alloc: Cold
        assert_eq!(m("^([^/]{2,3})", "日本語x").unwrap().1, vec![Some((0, 9))]); // alloc: Cold
        assert_eq!(m("^(.?)x", "éx").unwrap().1, vec![Some((0, 2))]); // alloc: Cold
        assert_eq!(m("a.b", "a\nb").unwrap().0, (0, 3));
    }

    #[test]
    fn leftmost_match() {
        assert_eq!(m("://([^/]+)/", "http://a.b/c").unwrap(), ((4, 11), vec![Some((7, 10))])); // alloc: Cold
        assert_eq!(m("b+", "aabbbc").unwrap().0, (2, 5));
        assert_eq!(m("x*", "abc").unwrap().0, (0, 0));
        assert_eq!(m("a*$", "bbb").unwrap().0, (3, 3));
        assert_eq!(m("([A-Za-z]+)/v?[0-9.]+", "Mozilla/5.0 (X11)").unwrap(), ((0, 11), vec![Some((0, 7))])); // alloc: Cold
        assert_eq!(m("([A-Za-z]+)/v?[0-9.]+", "x Opera/v12.1").unwrap(), ((2, 13), vec![Some((2, 7))])); // alloc: Cold
        assert!(m("([a-z]+)/[0-9]", "abc/def/ghi").is_none());
        assert_eq!(m("é(.)", "aéb").unwrap(), ((1, 4), vec![Some((3, 4))])); // alloc: Cold
        let pr = prog("[0-9]+");
        let Outcome::Match(f) = pr.find(b"a1b22c", 2) else { panic!("a match past 2") };
        assert_eq!((f.start, f.end), (3, 5));
        let mut out = [(-7i64, -7i64); 2];
        assert_eq!(pr.exec(b"a1b22c", 2, &mut out), Some(true));
        assert_eq!(out, [(3, 5), (-1, -1)]);
        assert_eq!(prog("^a").exec(b"ba", 1, &mut []), None);
    }

    #[test]
    fn anchors_and_tails() {
        assert_eq!(m("^", "abc").unwrap().0, (0, 0));
        assert_eq!(m("^$", "").unwrap().0, (0, 0));
        assert!(m("^$", "x").is_none());
        assert_eq!(m("^abc", "abcdef").unwrap().0, (0, 3));
        assert!(m("^abc$", "abcdef").is_none());
        assert_eq!(m("^a.*", "abc").unwrap().0, (0, 3));
        assert_eq!(m("^a.*$", "a").unwrap().0, (0, 1));
        assert!(m("^a.+$", "a").is_none());
        assert!(m("^a.+$", "ab").is_some());
        assert_eq!(m("c$", "abc").unwrap().0, (2, 3));
    }

    #[test]
    fn budget_refuses() {
        let pr = prog("^[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*c$");
        let hay = "ab".repeat(40);
        assert!(matches!(pr.find(hay.as_bytes(), 0), Outcome::Refused));
        assert_eq!(pr.exec(hay.as_bytes(), 0, &mut []), None);
        let pr = prog("[ab]*[ab]*[ab]*c");
        assert!(matches!(pr.find(hay.as_bytes(), 0), Outcome::Refused));
    }
}
