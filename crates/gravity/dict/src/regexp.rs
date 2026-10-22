//! `regexp_replace(entry, pattern, replacement)` per dictionary entry through the pattern program (`program`, the one the
//! per-row tier under RE2 runs too): the first match's replacement, the entry itself without a match. A pattern outside the
//! program's subset, or one whose greedy order PostgreSQL's longest match could leave (`Program::longest_is_first`),
//! stays on the function manager, where `required_literal` skips the entries that cannot match.
use crate::{BuildError, Region, View};
use gravity_abi::limits::REGEXP_MAX_PIECES as MAX_PIECES;
pub mod program;
pub use program::{Found, Outcome, Program};

/// The dictionary's program for `pattern`: the shared subset where PostgreSQL's longest match is the greedy order.
pub fn compile(pattern: &[u8]) -> Option<Program> {
    Program::compile(pattern).filter(Program::longest_is_first)
}

/// `compile`, and a pattern led by the embedded option `(?i)` (`regexp_replace`'s flag `'i'`, which lowerv4's derived
/// key spells so) compiled case-insensitively when `ascii_cases` answers that the input collation's regex case map takes
/// every ASCII letter to its ASCII twin (a C ctype, ICU, the builtin provider, the database's libc default; a libc
/// locale such as tr_TR, where `i`'s upper case is `İ`, is not).
pub fn compile_under(pattern: &[u8], ascii_cases: impl FnOnce() -> bool) -> Option<Program> {
    match pattern.strip_prefix(b"(?i)") {
        Some(rest) if ascii_cases() => Program::compile_folded(rest).filter(Program::longest_is_first),
        Some(_) => None,
        None => compile(pattern),
    }
}

/// What a `regexp_replace(s, pattern, r, flags)` call's flags leave for the pattern program (`flags_form`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlagsForm {
    /// `'g'`: every match is replaced (`replace_all`), not only the first.
    pub global: bool,
    /// `REG_ICASE` after the last `'i'` / `'c'` (the caller spells it as the embedded option `(?i)`).
    pub icase: bool,
}

/// `flags` as regexp.c `parse_re_flags` reads them (each letter in turn, so the last of `'i'` / `'c'`, of `'n'` / `'s'`
/// / `'p'` / `'w'`, of `'x'` / `'t'` wins) when the call then answers for `pattern` as the default advanced RE does
/// under `icase`, every match with `global`. `None` keeps the call on the function manager, which answers as
/// PostgreSQL does: an invalid letter (its 22023 raised where a row evaluates), a flavour other than the advanced RE
/// (`'b'`, `'e'`, `'q'`: no letter restores it), and a flag that can change what `pattern` matches:
/// - newline-sensitive (`'n'`, `'m'`, `'p'`, `'w'`) acts only on `.`, a negated bracket and the complemented class
///   escapes `\D` `\S` `\W` (`REG_NLSTOP`) and on `^` / `$` (`REG_NLANCH`): a pattern with none of these bytes (an
///   escaped `\.` counts too) means the same either way;
/// - expanded (`'x'`) drops white space and `#` comments: an ASCII pattern with no white space and no `#` means the same
///   (a non-ASCII character may be the collation's white space).
pub fn flags_form(flags: &[u8], pattern: &[u8]) -> Option<FlagsForm> {
    let (mut form, mut nl, mut expanded) = (FlagsForm { global: false, icase: false }, false, false);
    for &c in flags {
        match c {
            b'g' => form.global = true,
            b'i' => form.icase = true,
            b'c' => form.icase = false,
            b'm' | b'n' | b'p' | b'w' => nl = true,
            b's' => nl = false,
            b'x' => expanded = true,
            b't' => expanded = false,
            _ => return None,
        }
    }
    let newline_free = || {
        let mut i = 0;
        while i < pattern.len() {
            match pattern[i] {
                b'.' | b'^' | b'$' => return false,
                b'\\' if matches!(pattern.get(i + 1), Some(b'D' | b'S' | b'W')) => return false,
                b'\\' => i += 2,
                _ => i += 1,
            }
        }
        true
    };
    let plain = || pattern.iter().all(|&b| b.is_ascii() && !b.is_ascii_whitespace() && b != 0x0B && b != b'#');
    ((!nl || newline_free()) && (!expanded || plain())).then_some(form)
}

#[inline(always)]
fn continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// The last in-place replacement remembered (`replace_first_memo`): a subject that starts with
/// the bytes the previous run compared (`prefix`), when that run never consulted the subject's
/// end, takes the same branches to the same groups — the derived value is the same slice of the
/// new subject. Dictionary entries come in byte order, so consecutive entries of one host share
/// the prefix: one `memcmp` in place of the program.
pub struct Memo {
    prefix: [u8; MEMO_BYTES],
    plen: usize,
    cap: (usize, usize),
    /// The census: subjects answered from the prefix, subjects that ran the program.
    pub hits: u64,
    pub runs: u64,
    /// The prefixes remembered: a hit's value is the previous subject's when no miss since the last one left it unset.
    pub sets: u64,
}

pub const MEMO_BYTES: usize = 256;

impl Memo {
    pub const fn new() -> Memo {
        Memo { prefix: [0; MEMO_BYTES], plen: usize::MAX, cap: (0, 0), hits: 0, runs: 0, sets: 0 }
    }
}

impl Default for Memo {
    fn default() -> Memo {
        Memo::new()
    }
}

/// `replace_first` for a single-group template over a program anchored at both ends (the `in_place` plan), with the
/// previous subject's decision remembered in `memo`; `None` where the program refused the subject (its budget).
/// Always inlined: its callers pull it once per dictionary entry (gravity_hosts text_hosts `derived_seq`, the derived
/// table's one loop), and out of line the call's argument and result traffic per entry cost a GROUP BY over
/// `regexp_replace` of a dictionary text column 7 % more instructions (D3424).
#[inline(always)]
pub fn replace_first_memo(prog: &Program, tmpl: &Template, rep: &[u8], hay: &[u8], region: &mut Region, memo: &mut Memo) -> Result<Option<View>, BuildError> {
    if memo.plen <= hay.len() && hay[..memo.plen] == memo.prefix[..memo.plen] {
        memo.hits += 1;
        return Ok(Some(View::of(&hay[memo.cap.0..memo.cap.1])));
    }
    memo.runs += 1;
    if !(tmpl.single_group() && prog.anchored() && prog.anchored_to_end()) {
        return replace_first(prog, tmpl, rep, hay, region);
    }
    let g = match tmpl.pieces[0] {
        Piece::Group(g) => g as usize,
        _ => 0,
    };
    let f = match prog.find_group(hay, g) {
        Outcome::Match(f) if f.end == hay.len() => f,
        Outcome::Match(_) => return replace_first(prog, tmpl, rep, hay, region),
        Outcome::NoMatch => return Ok(Some(View::of(hay))),
        Outcome::Refused => return Ok(None),
    };
    let group = f.group;
    // remembered when the decision and the group lie inside the compared prefix
    if !f.saw_end && f.read <= MEMO_BYTES && f.read <= hay.len() && group.1 <= f.read && matches!(tmpl.pieces[0], Piece::Group(_)) {
        memo.prefix[..f.read].copy_from_slice(&hay[..f.read]);
        memo.plen = f.read;
        memo.cap = group;
        memo.sets += 1;
    }
    Ok(Some(View::of(&hay[group.0..group.1])))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Piece {
    Bytes(u16, u16),
    Group(u8),
    Whole,
}

/// The replacement text in `regexp_replace`'s convention (`replace_text_regexp`): `\1`..`\9` a
/// group (empty when the pattern has no such group), `\&` the whole match, `\\` a backslash,
/// any other backslash literal.
pub struct Template {
    pieces: [Piece; MAX_PIECES],
    n: usize,
}

impl Template {
    pub fn parse(rep: &[u8]) -> Option<Template> {
        if rep.len() > u16::MAX as usize {
            return None;
        }
        let mut t = Template { pieces: [Piece::Whole; MAX_PIECES], n: 0 };
        let (mut i, mut start) = (0usize, 0usize);
        while i < rep.len() {
            if rep[i] != b'\\' {
                i += 1;
                continue;
            }
            t.bytes(start, i)?;
            let (piece, step) = match rep.get(i + 1) {
                Some(c @ b'1'..=b'9') => (Piece::Group(c - b'0'), 2),
                Some(b'&') => (Piece::Whole, 2),
                Some(b'\\') => (Piece::Bytes(i as u16, 1), 2),
                _ => (Piece::Bytes(i as u16, 1), 1),
            };
            t.push(piece)?;
            i += step;
            start = i;
        }
        t.bytes(start, rep.len())?;
        Some(t)
    }

    fn push(&mut self, p: Piece) -> Option<()> {
        if self.n >= MAX_PIECES {
            return None;
        }
        self.pieces[self.n] = p;
        self.n += 1;
        Some(())
    }

    fn bytes(&mut self, from: usize, to: usize) -> Option<()> {
        if to > from {
            return self.push(Piece::Bytes(from as u16, (to - from) as u16));
        }
        Some(())
    }

    /// One group reference and nothing else: over a match of the whole subject the result is a subslice of it.
    pub fn single_group(&self) -> bool {
        self.n == 1 && matches!(self.pieces[0], Piece::Group(_) | Piece::Whole)
    }

    /// The replacement's length over match `f` of `hay`.
    fn len_over(&self, f: &Found, hay: &[u8]) -> usize {
        self.pieces[..self.n]
            .iter()
            .map(|p| match *p {
                Piece::Bytes(_, l) => l as usize,
                Piece::Group(g) => group_of(f, hay, g).len(),
                Piece::Whole => f.end - f.start,
            })
            .sum()
    }

    /// The replacement over match `f` of `hay`, piece by piece (`rep` the replacement text the template was parsed from).
    #[inline(always)]
    fn emit(&self, rep: &[u8], f: &Found, hay: &[u8], put: &mut impl FnMut(&[u8])) {
        for p in &self.pieces[..self.n] {
            match *p {
                Piece::Bytes(o, l) => put(&rep[o as usize..o as usize + l as usize]),
                Piece::Group(g) => put(group_of(f, hay, g)),
                Piece::Whole => put(&hay[f.start..f.end]),
            }
        }
    }
}

/// `regexp_replace(hay, pattern, replacement)` (the first match): the subject itself when the pattern does not match; a
/// subslice of `hay` for a single-group template over a match of the whole subject; otherwise the result copied into
/// `region`. `None`: the program refused the subject (its budget); the caller's engine answers it.
pub fn replace_first(prog: &Program, tmpl: &Template, rep: &[u8], hay: &[u8], region: &mut Region) -> Result<Option<View>, BuildError> {
    let f = match prog.find(hay, 0) {
        Outcome::Match(f) => f,
        Outcome::NoMatch => return Ok(Some(View::of(hay))),
        Outcome::Refused => return Ok(None),
    };
    replace_one(tmpl, rep, &f, hay, region).map(Some)
}

/// `hay` with match `f` replaced: a subslice of `hay` for a single-group template over a match of the whole subject,
/// otherwise the result copied into `region` in one piece.
#[inline(always)]
fn replace_one(tmpl: &Template, rep: &[u8], f: &Found, hay: &[u8], region: &mut Region) -> Result<View, BuildError> {
    let whole = &hay[f.start..f.end];
    if tmpl.single_group() && f.start == 0 && f.end == hay.len() {
        return Ok(View::of(match tmpl.pieces[0] {
            Piece::Group(g) => group_of(f, hay, g),
            _ => whole,
        }));
    }
    let total = hay.len() - whole.len() + tmpl.len_over(f, hay);
    let dst = region.alloc(total, 1)?;
    let mut at = 0usize;
    // SAFETY: `dst` holds `total` bytes, the sum of the pieces written below.
    let mut put = |b: &[u8]| unsafe {
        core::ptr::copy_nonoverlapping(b.as_ptr(), dst.add(at), b.len());
        at += b.len();
    };
    put(&hay[..f.start]);
    tmpl.emit(rep, f, hay, &mut put);
    put(&hay[f.end..]);
    Ok(View { ptr: dst, len: total as u32 })
}

/// `regexp_replace(hay, pattern, replacement, 'g')`: every match replaced, in PostgreSQL's order (varlena.c
/// `replace_text_regexp` with `n = 0`): each search starts where the previous match ended, one character further after
/// an empty match, up to and including the subject's end (an empty match there counts). The subject itself when nothing
/// matches; otherwise the result built in `region`, the text between matches and each match's replacement written in
/// turn (nothing else allocates in the region meanwhile, so the pieces lie end to end from the first). An anchored
/// program matches at the subject's start only (`^` never holds past it: no newline-sensitive flag in the subset), so
/// it answers as `replace_first`. `None`: the program refused a search (its budget); the caller's engine answers.
pub fn replace_all(prog: &Program, tmpl: &Template, rep: &[u8], hay: &[u8], region: &mut Region) -> Result<Option<View>, BuildError> {
    let mut f = match prog.find(hay, 0) {
        Outcome::Match(f) => f,
        Outcome::NoMatch => return Ok(Some(View::of(hay))),
        Outcome::Refused => return Ok(None),
    };
    // one match: `replace_first`'s result, built in one piece
    let mut after = match find_after(prog, hay, &f) {
        Outcome::Match(n) => n,
        Outcome::NoMatch => return replace_one(tmpl, rep, &f, hay, region).map(Some),
        Outcome::Refused => return Ok(None),
    };
    let mut out = Append { first: core::ptr::null_mut(), len: 0, err: None };
    let mut copied = 0usize;
    // bounded("other: each turn consumes at least one character of the subject, or a match ends the loop at its end")
    loop {
        out.put(region, &hay[copied..f.start]);
        tmpl.emit(rep, &f, hay, &mut |b| out.put(region, b));
        copied = f.end;
        f = after;
        after = match find_after(prog, hay, &f) {
            Outcome::Match(n) => n,
            Outcome::NoMatch => break,
            Outcome::Refused => return Ok(None),
        };
    }
    out.put(region, &hay[copied..f.start]);
    tmpl.emit(rep, &f, hay, &mut |b| out.put(region, b));
    copied = f.end;
    out.put(region, &hay[copied..]);
    if let Some(e) = out.err {
        return Err(e);
    }
    Ok(Some(View { ptr: if out.first.is_null() { hay.as_ptr() } else { out.first }, len: out.len as u32 }))
}

/// PostgreSQL's next search after match `f` (`replace_text_regexp`): from its end, one character further after an empty
/// match, none past the subject's end.
#[inline(always)]
fn find_after(prog: &Program, hay: &[u8], f: &Found) -> Outcome {
    let mut next = f.end;
    if f.start == f.end {
        if next >= hay.len() {
            return Outcome::NoMatch;
        }
        next = (next + program::utf8_char_len(hay[next])).min(hay.len());
    }
    if next >= hay.len() && prog.needs_byte() {
        return Outcome::NoMatch;
    }
    prog.find(hay, next)
}

/// `replace_all`'s output: pieces appended to the region end to end, the first piece's address kept; a region too
/// short stops the writes and keeps its error (`RegionFull`: the caller grows the region and builds again).
struct Append {
    first: *mut u8,
    len: usize,
    err: Option<BuildError>,
}

impl Append {
    #[inline(always)]
    fn put(&mut self, region: &mut Region, b: &[u8]) {
        if self.err.is_some() || (b.is_empty() && !self.first.is_null()) {
            return;
        }
        match region.alloc(b.len(), 1) {
            Ok(p) => {
                if self.first.is_null() {
                    self.first = p;
                }
                // SAFETY: `p` holds `b.len()` bytes just reserved; `b` (the subject or the replacement) lies outside the region's free end.
                unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len()) };
                self.len += b.len();
            }
            Err(e) => self.err = Some(e),
        }
    }
}

fn group_of<'h>(f: &Found, hay: &'h [u8], g: u8) -> &'h [u8] {
    f.group(g as usize).map_or(&hay[..0], |(s, e)| &hay[s..e])
}

/// A byte run every match of `pattern` (an ARE) contains, as a range of the pattern: the
/// longest top-level literal run outside groups, classes and quantifiers (ClickHouse's
/// `OptimizedRegularExpression` prefilter). `None` when nothing is certain — any alternation,
/// an escape longer than two bytes (`multi_byte_escape`), an embedded option, a director prefix.
/// An escape of two bytes ends the run (`\.` and `\d` alike); inside a bracket a backslash
/// escapes the next byte, so `\]` does not close it (ARE brackets take escapes).
pub fn required_literal(pattern: &[u8]) -> Option<(usize, usize)> {
    if pattern.starts_with(b"***") || pattern.contains(&b'|') {
        return None;
    }
    let (mut best, mut run_start, mut depth) = (None::<(usize, usize)>, None::<usize>, 0usize);
    let mut i = 0;
    let close = |run_start: &mut Option<usize>, to: usize, best: &mut Option<(usize, usize)>| {
        if let Some(s) = run_start.take() {
            if to > s && best.is_none_or(|(bs, be)| to - s > be - bs) {
                *best = Some((s, to));
            }
        }
    };
    while i < pattern.len() {
        let b = pattern[i];
        match b {
            b'\\' => {
                // D3570: `\u00a0` is one character spelled over six bytes; stepping two left `00a0` read as a literal
                // the subject must hold, so a matching entry was skipped as unchanged
                if multi_byte_escape(pattern.get(i + 1).copied()) {
                    return None;
                }
                close(&mut run_start, i, &mut best);
                i += 2;
            }
            b'(' => {
                if pattern.get(i + 1) == Some(&b'?') && pattern.get(i + 2) != Some(&b':') {
                    return None;
                }
                close(&mut run_start, i, &mut best);
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b'[' => {
                close(&mut run_start, i, &mut best);
                i += 1;
                if pattern.get(i) == Some(&b'^') {
                    i += 1;
                }
                if pattern.get(i) == Some(&b']') {
                    i += 1;
                }
                loop {
                    match pattern.get(i) {
                        None => return None,
                        Some(b']') => break,
                        // D3570: an escape inside the bracket (`[-_\]]`): the escaped byte is a member, never its end
                        Some(b'\\') => i += 2,
                        Some(b'[') => {
                            let end = pattern[i..].windows(2).position(|w| w[1] == b']' && matches!(w[0], b':' | b'.' | b'='))?;
                            i += end + 2;
                        }
                        Some(_) => i += 1,
                    }
                }
                i += 1;
            }
            b'*' | b'+' | b'?' | b'{' => {
                let at_least_once = b == b'+' || (b == b'{' && pattern.get(i + 1).is_some_and(|&d| (b'1'..=b'9').contains(&d)));
                if let Some(s) = run_start {
                    let last = if at_least_once { i } else { pattern[s..i].iter().rposition(|&c| !continuation(c)).map_or(i, |p| s + p) };
                    close(&mut run_start, last, &mut best);
                }
                if b == b'{' {
                    i = i + pattern[i..].iter().position(|&c| c == b'}')? + 1;
                } else {
                    i += 1;
                }
            }
            b'.' | b'^' | b'$' => {
                close(&mut run_start, i, &mut best);
                i += 1;
            }
            _ => {
                if depth == 0 && run_start.is_none() {
                    run_start = Some(i);
                }
                i += 1;
            }
        }
    }
    close(&mut run_start, pattern.len(), &mut best);
    best.filter(|&(s, e)| e > s)
}

/// An ARE escape that runs past the byte after the backslash, or a pattern that ends in a backslash: a character entry
/// (`\u` 4 hex digits, `\U` 8, `\x` any number, `\0` and its octal digits, `\c` and a character) or a back reference (`\1`
/// ..). Every other escape is two bytes: a character class (`\d`), a constraint (`\y`), an escaped character (`\.`, `\n`).
fn multi_byte_escape(next: Option<u8>) -> bool {
    next.is_none_or(|c| matches!(c, b'u' | b'U' | b'x' | b'c') || c.is_ascii_digit())
}
