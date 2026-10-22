//! FAIL-CLOSED compile-time compatibility classifier for the auto dispatch:
//! a pattern is admitted to RE2 only when every construct in it is on the
//! proven-equivalent whitelist (RE2 in POSIX longest-match mode vs the
//! Spencer ARE port). Anything unrecognized, ambiguous, or known-divergent
//! (docs/design/regex-engine-ab-verdict.md) classifies as Spencer.
//!
//! Rejected by construction (the documented delta list):
//! - backreferences and lookaround (`\1`..`\9`, `(?=`, `(?!`, `(?<`);
//! - ctype/collation-sensitive classes and escapes (`\w \d \b \y \Y \W \S
//!   \D \B \Z \A`, `[[:alpha:]]`, `[[=x=]]`, `[[.x.]]`), except the exact
//!   translations below;
//! - non-greedy quantifiers (Spencer preference rules vs leftmost-first);
//! - REG_ICASE (Unicode simple folding vs collation-driven pg_wc_tolower),
//!   REG_EXPANDED, REG_NLSTOP/REG_NLANCH newline modes;
//! - non-ARE modes other than 'q' (REG_QUOTE);
//! - escapes inside bracket expressions, POSIX named classes, collating
//!   elements, equivalence classes, non-ASCII range endpoints;
//! - repeat bounds above 255 (Spencer's DUPMAX) or malformed bounds;
//! - inline option/director groups (`(?i)`, `***:`);
//! - non-UTF8 databases and patterns that are not valid UTF-8 or contain
//!   NUL (Spencer's pattern view stops at the first NUL);
//! Additionally, capture POSITIONS are only trusted ("capture-safe") when
//! the parse of every leftmost-longest match is forced to agree. Spencer
//! (regcomp.c parseqatom: concat(prefix, concat(capture, rest)); regexec.c
//! ccondissect) resolves captures by giving each segment between capture
//! boundaries, left to right, its LONGEST extent consistent with the whole
//! match; RE2 and the program tier keep the highest Perl-priority parse,
//! which maximizes quantifier iteration counts left to right. The two
//! orders coincide when each segment (every one but the last, after the
//! final capture) has at most one quantifier whose count is not forced by
//! the input, and no other variable-count quantifier after it: then the
//! segment's length is strictly increasing in that single count (see
//! `captures_determined`). Anything else — quantified captures, nested
//! captures or captures inside groups, alternation alongside captures,
//! multi-quantifier segments such as `a?(?:ab)?(b?)` — still dispatches for
//! whole-match-only uses (leftmost-longest spans are uniquely defined and
//! agree), but every submatch-consuming call falls to Spencer.
//!
//! D3121: three escapes are admitted as exact translations, under a per-call
//! guard. `\s` becomes `[\x09-\x0D\x20]` (RE2's own `\s` lacks `\v`), and
//! `\m` / `\M` become `\b` where every character the adjacent item can put
//! next to them is an ASCII word character (`\m(inc|llc)`, `(inc|llc)\M`; see
//! `boundaries_exact`): with a word character on one side, `\b` holds exactly
//! where the other side is not one, which is Spencer's start / end of word.
//! Elsewhere such a pattern stays on Spencer. RE2's classes are ASCII and
//! Spencer's follow the collation, so the translated pattern runs RE2 only
//! over a subject that is ASCII and has no FS..US (0x1C-0x1F, which ICU's
//! u_isspace counts as space); on ASCII every provider agrees on [[:space:]]
//! and [[:alnum:]_] otherwise. `Classification::ascii_subject` carries it
//! to `Re2Pattern::subject_ok`.
//!
//! - patterns beyond the complexity budget (MAX_PATTERN_BYTES /
//!   MAX_QUANTIFIERS): Spencer enforces NFA state/arc limits ("regular
//!   expression is too complex") that RE2 does not share, so a large
//!   admitted pattern could succeed under RE2 where Spencer errors (caught
//!   live by the regex regress suite's repeat('x*y*z*', 1000) case). The
//!   budget keeps admitted patterns far below Spencer's limits.

use ::regex_spencer::{REG_ADVANCED, REG_NOSUB, REG_QUOTE};

const PG_UTF8: i32 = wchar::PG_UTF8;

const MAX_PATTERN_BYTES: usize = 256;
const MAX_QUANTIFIERS: u32 = 32;
const MAX_GROUP_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compat {
    Incompatible,
    // Whole-match spans provably agree; capture positions do not.
    WholeMatch,
    // Capture positions provably agree too.
    CaptureSafe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Classification {
    pub tier: Compat,
    // POSIX longest-match mode disambiguates alternation (`a|ab`) and any
    // greedy quantifier whose first-match preference can leave bytes a later
    // optional atom would add (`a*(?:ab)?` on "aaab"). Only a pattern pinned
    // at both ends (`^...$`) has a unique span, where first-match order is
    // the leftmost-longest choice and the faster engine configuration (and
    // RE2's one-pass capture paths) stays provably identical.
    pub needs_longest: bool,
    // D3121: the pattern uses a translated escape (\s \m \M): RE2 runs
    // `re2_pattern(pattern)`, and only over an ASCII subject (subject_ok).
    pub ascii_subject: bool,
}

const INCOMPATIBLE: Classification =
    Classification { tier: Compat::Incompatible, needs_longest: false, ascii_subject: false };

pub fn classify(pattern: &[u8], cflags: i32) -> Classification {
    if pattern.len() > MAX_PATTERN_BYTES {
        return INCOMPATIBLE;
    }
    if mbutils::GetDatabaseEncoding() != PG_UTF8 {
        return INCOMPATIBLE;
    }
    // REG_NOSUB is an execution hint callers OR in, not a semantic mode.
    let base = cflags & !REG_NOSUB;
    let quoted = base == REG_QUOTE;
    if !quoted && base != REG_ADVANCED {
        return INCOMPATIBLE;
    }
    // NUL is valid UTF-8, but Spencer compiles the pattern through
    // pg_mb2wchar (stops at the first NUL) while RE2 would compile the full
    // byte string.
    if core::str::from_utf8(pattern).is_err() || pattern.contains(&0) {
        return INCOMPATIBLE;
    }
    if quoted {
        return Classification { tier: Compat::CaptureSafe, needs_longest: false, ascii_subject: false };
    }
    scan_are(pattern)
}

pub fn re2_compatible(pattern: &[u8], cflags: i32) -> bool {
    classify(pattern, cflags).tier != Compat::Incompatible
}

fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

// Escapes equivalent as pure literals in both engines: the C-style control
// escapes plus escaped ASCII punctuation (Spencer: any escaped non-word
// character is that literal; RE2 agrees for these, and where RE2 instead
// errors the auto path falls back to Spencer at compile).
fn literal_escape_ok(c: u8) -> bool {
    matches!(c, b'n' | b't' | b'r' | b'f' | b'v' | b'a')
        || (c.is_ascii() && !c.is_ascii_alphanumeric())
}

// Parses {m}, {m,}, {m,n} with m <= n <= 255; returns the index just past
// '}' or None when the brace is not a well-formed bound (Spencer and RE2
// disagree on literal-brace fallback, so malformed means incompatible).
fn parse_bound(pat: &[u8], mut i: usize) -> Option<usize> {
    debug_assert_eq!(pat[i], b'{');
    i += 1;
    let mut m: u32 = 0;
    let m_start = i;
    while i < pat.len() && pat[i].is_ascii_digit() {
        m = m * 10 + (pat[i] - b'0') as u32;
        if m > 255 {
            return None;
        }
        i += 1;
    }
    if i == m_start || (i - m_start > 1 && pat[m_start] == b'0') {
        return None;
    }
    if i < pat.len() && pat[i] == b'}' {
        return Some(i + 1);
    }
    if i >= pat.len() || pat[i] != b',' {
        return None;
    }
    i += 1;
    if i < pat.len() && pat[i] == b'}' {
        return Some(i + 1);
    }
    let mut n: u32 = 0;
    let n_start = i;
    while i < pat.len() && pat[i].is_ascii_digit() {
        n = n * 10 + (pat[i] - b'0') as u32;
        if n > 255 {
            return None;
        }
        i += 1;
    }
    if i == n_start
        || (i - n_start > 1 && pat[n_start] == b'0')
        || n < m
        || i >= pat.len()
        || pat[i] != b'}'
    {
        return None;
    }
    Some(i + 1)
}

// Returns the index just past the closing ']' when the bracket expression is
// on the whitelist: plain members and ASCII-endpoint ranges only.
fn parse_bracket(pat: &[u8], mut i: usize) -> Option<usize> {
    debug_assert_eq!(pat[i], b'[');
    i += 1;
    if i < pat.len() && pat[i] == b'^' {
        i += 1;
    }
    // Leading ']' is a member under POSIX but an error under RE2: reject.
    // prev_ascii: Some(true) after an ASCII member, Some(false) after a
    // multibyte member, None at the start or after a range/dash.
    let mut prev_ascii: Option<bool> = None;
    let mut any_member = false;
    while i < pat.len() {
        match pat[i] {
            b']' if any_member => return Some(i + 1),
            b']' => return None,
            b'\\' => return None,
            b'[' if i + 1 < pat.len() && matches!(pat[i + 1], b':' | b'.' | b'=') => {
                return None
            }
            b'-' => {
                // Literal at start or end; otherwise a range: both endpoints
                // must be ASCII (code-point ranges match; wider left closed).
                if i + 1 < pat.len() && pat[i + 1] == b']' {
                    i += 1;
                    any_member = true;
                    prev_ascii = None;
                } else if prev_ascii == Some(true) {
                    i += 1;
                    if i >= pat.len() || !pat[i].is_ascii() || matches!(pat[i], b'\\' | b'[') {
                        return None;
                    }
                    i += 1;
                    prev_ascii = None;
                } else if prev_ascii.is_none() && !any_member {
                    i += 1;
                    any_member = true;
                    prev_ascii = None;
                } else {
                    return None;
                }
            }
            b => {
                let len = utf8_char_len(b);
                if i + len > pat.len() {
                    return None;
                }
                prev_ascii = Some(len == 1);
                any_member = true;
                i += len;
            }
        }
    }
    None
}

fn scan_are(pat: &[u8]) -> Classification {
    let mut i = 0usize;
    let mut nquant = 0u32;
    let mut capture_safe = true;
    let mut has_capture = false;
    let mut has_alternation = false;
    // D3121: a translated escape (\s \m \M) appears.
    let mut translated = false;
    // True when the previous item is an atom a quantifier may apply to.
    let mut quantifiable = false;
    // Did the just-closed atom's subtree contain a capturing group? A
    // quantifier over such a subtree makes capture positions untrusted
    // (last-iteration submatch semantics diverge).
    let mut last_atom_captures = false;
    // Per open group: (is_capturing, subtree_contains_capture_so_far).
    let mut stack = [(false, false); MAX_GROUP_DEPTH];
    let mut depth = 0usize;

    while i < pat.len() {
        match pat[i] {
            b'\\' => {
                match pat.get(i + 1) {
                    // D3121: the space class, translated to its ASCII members.
                    Some(b's') => {
                        translated = true;
                        quantifiable = true;
                    }
                    // D3121: start / end of word, zero-width (a quantifier on
                    // one is Spencer's REG_BADRPT, so it stays off-list).
                    Some(b'm' | b'M') => {
                        translated = true;
                        quantifiable = false;
                    }
                    Some(&c) if literal_escape_ok(c) => quantifiable = true,
                    _ => return INCOMPATIBLE,
                }
                i += 2;
                last_atom_captures = false;
            }
            b'[' => match parse_bracket(pat, i) {
                Some(next) => {
                    i = next;
                    quantifiable = true;
                    last_atom_captures = false;
                }
                None => return INCOMPATIBLE,
            },
            b'(' => {
                let capturing;
                if i + 1 < pat.len() && pat[i + 1] == b'?' {
                    // Only the non-capturing group; every other (?...) form
                    // (inline options, lookaround, named) is off-list.
                    if i + 2 >= pat.len() || pat[i + 2] != b':' {
                        return INCOMPATIBLE;
                    }
                    capturing = false;
                    i += 3;
                } else {
                    capturing = true;
                    has_capture = true;
                    i += 1;
                }
                if depth >= MAX_GROUP_DEPTH {
                    return INCOMPATIBLE;
                }
                stack[depth] = (capturing, false);
                depth += 1;
                quantifiable = false;
                last_atom_captures = false;
            }
            b')' => {
                if depth == 0 {
                    return INCOMPATIBLE;
                }
                depth -= 1;
                let (capturing, contains) = stack[depth];
                last_atom_captures = capturing || contains;
                if last_atom_captures && depth > 0 {
                    stack[depth - 1].1 = true;
                }
                i += 1;
                quantifiable = true;
            }
            b'*' | b'+' | b'?' => {
                if !quantifiable {
                    return INCOMPATIBLE;
                }
                nquant += 1;
                if nquant > MAX_QUANTIFIERS {
                    return INCOMPATIBLE;
                }
                if last_atom_captures {
                    capture_safe = false;
                }
                i += 1;
                if i < pat.len() && pat[i] == b'?' {
                    return INCOMPATIBLE;
                }
                quantifiable = false;
                last_atom_captures = false;
            }
            b'{' => {
                if !quantifiable {
                    return INCOMPATIBLE;
                }
                nquant += 1;
                if nquant > MAX_QUANTIFIERS {
                    return INCOMPATIBLE;
                }
                if last_atom_captures {
                    capture_safe = false;
                }
                match parse_bound(pat, i) {
                    Some(next) => i = next,
                    None => return INCOMPATIBLE,
                }
                if i < pat.len() && pat[i] == b'?' {
                    return INCOMPATIBLE;
                }
                quantifiable = false;
                last_atom_captures = false;
            }
            b'|' => {
                has_alternation = true;
                i += 1;
                quantifiable = false;
                last_atom_captures = false;
            }
            b'^' | b'$' => {
                i += 1;
                quantifiable = false;
                last_atom_captures = false;
            }
            b'.' => {
                i += 1;
                quantifiable = true;
                last_atom_captures = false;
            }
            b => {
                let len = utf8_char_len(b);
                if i + len > pat.len() {
                    return INCOMPATIBLE;
                }
                i += len;
                quantifiable = true;
                last_atom_captures = false;
            }
        }
    }
    if depth != 0 {
        return INCOMPATIBLE;
    }
    if translated && !boundaries_exact(pat) {
        return INCOMPATIBLE;
    }
    let tier = if capture_safe
        && !(has_capture && (has_alternation || !captures_determined(pat)))
    {
        Compat::CaptureSafe
    } else {
        Compat::WholeMatch
    };
    let pinned = pat.first() == Some(&b'^') && ends_with_anchor(pat);
    Classification {
        tier,
        needs_longest: has_alternation || (nquant > 0 && !pinned),
        ascii_subject: translated,
    }
}

// ASCII members of Spencer's \s ([[:space:]] on ASCII: HT LF VT FF CR SP).
const SPACE: u128 = (0x1F << 0x09) | (1 << 0x20);

// ASCII word characters ([[:alnum:]_]), the same in every collation provider.
const WORD: u128 = (0x3FF << b'0') | (0x3FF_FFFF << b'A') | (1 << b'_') | (0x3FF_FFFF << b'a');

// What an item puts at the edges of every string it matches.
#[derive(Clone, Copy)]
struct Edges {
    // Every match is non-empty and starts with an ASCII word character.
    first_word: bool,
    // Every match is non-empty and ends with an ASCII word character.
    last_word: bool,
}

// D3121: whether every \m has an item after it, in its branch, whose matches
// all start with a word character, and every \M an item before it whose
// matches all end with one. Then \b is exact for both: the known side is a
// word character, so \b holds where the other side is not, which is \m / \M.
// Runs on a pattern scan_are already admitted (its grammar, depth and bounds).
fn boundaries_exact(pat: &[u8]) -> bool {
    let mut b = Bounds { pat, i: 0 };
    b.alt().is_some() && b.i == pat.len()
}

struct Bounds<'a> {
    pat: &'a [u8],
    i: usize,
}

impl Bounds<'_> {
    // branch ('|' branch)*, up to a ')' or the end.
    fn alt(&mut self) -> Option<Edges> {
        let mut e = self.branch()?;
        while self.pat.get(self.i) == Some(&b'|') {
            self.i += 1;
            let b = self.branch()?;
            e.first_word &= b.first_word;
            e.last_word &= b.last_word;
        }
        Some(e)
    }

    // A concatenation; None when a \m / \M in it is not exact.
    fn branch(&mut self) -> Option<Edges> {
        let mut first: Option<bool> = None;
        // The previous item's last_word (false at the start and after a zero-width item).
        let mut prev_last = false;
        let mut want_word_next = false;
        loop {
            let zero_width = match (self.pat.get(self.i), self.pat.get(self.i + 1)) {
                (None | Some(b'|' | b')'), _) => break,
                (Some(b'\\'), Some(b'm')) => {
                    if want_word_next {
                        return None;
                    }
                    want_word_next = true;
                    2
                }
                (Some(b'\\'), Some(b'M')) => {
                    if want_word_next || !prev_last {
                        return None;
                    }
                    2
                }
                (Some(b'^' | b'$'), _) => {
                    if want_word_next {
                        return None;
                    }
                    1
                }
                _ => 0,
            };
            if zero_width > 0 {
                self.i += zero_width;
                first.get_or_insert(false);
                prev_last = false;
                continue;
            }
            let e = self.item()?;
            if want_word_next && !e.first_word {
                return None;
            }
            want_word_next = false;
            first.get_or_insert(e.first_word);
            prev_last = e.last_word;
        }
        if want_word_next {
            return None;
        }
        Some(Edges { first_word: first.unwrap_or(false), last_word: prev_last })
    }

    // One atom or group with its quantifier; a quantifier that admits zero
    // iterations makes the item possibly empty, so no edge is known.
    fn item(&mut self) -> Option<Edges> {
        let (mut e, next) = if self.pat[self.i] == b'(' {
            self.i += if self.pat.get(self.i + 1) == Some(&b'?') { 3 } else { 1 };
            let e = self.alt()?;
            if self.pat.get(self.i) != Some(&b')') {
                return None;
            }
            (e, self.i + 1)
        } else {
            let (set, next) = single_atom(self.pat, self.i)?;
            let w = set.ascii != 0 && set.ascii & !WORD == 0 && !set.non_ascii;
            (Edges { first_word: w, last_word: w }, next)
        };
        let (min, _, after) = quantifier(self.pat, next);
        if min == 0 {
            e = Edges { first_word: false, last_word: false };
        }
        self.i = after;
        Some(e)
    }
}

// D3121: the pattern RE2 compiles for an admitted one: \s as its ASCII
// members, \m and \M as \b (boundaries_exact held); everything else verbatim.
#[cfg_attr(not(have_re2), allow(dead_code))]
pub fn re2_pattern(pat: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = out.try_reserve_exact(pat.len() + 16);
    let mut i = 0usize;
    while i < pat.len() {
        match pat[i] {
            b'\\' if i + 1 < pat.len() => {
                match pat[i + 1] {
                    b's' => out.extend_from_slice(br"[\x09-\x0D\x20]"),
                    b'm' | b'M' => out.extend_from_slice(br"\b"),
                    c => out.extend_from_slice(&[b'\\', c]),
                }
                i += 2;
            }
            // A bracket expression holds no escapes (parse_bracket); copied whole.
            b'[' => {
                let end = parse_bracket(pat, i).unwrap_or(pat.len());
                out.extend_from_slice(&pat[i..end]);
                i = end;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

// First-character set of a consuming item, for the forced-count test.
// Multibyte members are not tracked individually: two sets that both admit
// any non-ASCII character are treated as overlapping.
#[derive(Clone, Copy)]
struct FirstSet {
    ascii: u128,
    non_ascii: bool,
}

impl FirstSet {
    const ANY: FirstSet = FirstSet { ascii: u128::MAX, non_ascii: true };

    fn byte(b: u8) -> FirstSet {
        FirstSet { ascii: 1u128 << b, non_ascii: false }
    }

    fn disjoint(self, other: FirstSet) -> bool {
        self.ascii & other.ascii == 0 && !(self.non_ascii && other.non_ascii)
    }
}

fn escape_byte(c: u8) -> u8 {
    match c {
        b'n' => b'\n',
        b't' => b'\t',
        b'r' => b'\r',
        b'f' => 0x0C,
        b'v' => 0x0B,
        b'a' => 0x07,
        _ => c,
    }
}

// The members of a whitelisted bracket expression (parse_bracket already
// accepted it) starting at pat[i] == '['.
fn bracket_set(pat: &[u8], mut i: usize) -> Option<(FirstSet, usize)> {
    let end = parse_bracket(pat, i)?;
    i += 1;
    let negated = pat[i] == b'^';
    if negated {
        i += 1;
    }
    let mut set = FirstSet { ascii: 0, non_ascii: false };
    let mut prev: Option<u8> = None;
    while i < end - 1 {
        let b = pat[i];
        if b == b'-' && prev.is_some() && i + 1 < end - 1 {
            let (lo, hi) = (prev?, pat[i + 1]);
            if hi < lo {
                return None;
            }
            for c in lo..=hi {
                set.ascii |= 1u128 << c;
            }
            i += 2;
            prev = None;
        } else if b.is_ascii() {
            set.ascii |= 1u128 << b;
            prev = Some(b);
            i += 1;
        } else {
            set.non_ascii = true;
            prev = None;
            i += utf8_char_len(b);
        }
    }
    if negated {
        set = FirstSet { ascii: !set.ascii, non_ascii: true };
    }
    Some((set, end))
}

// One single-character atom at pat[i]: (first set, index past it).
fn single_atom(pat: &[u8], i: usize) -> Option<(FirstSet, usize)> {
    match pat[i] {
        b'\\' => match *pat.get(i + 1)? {
            b's' => Some((FirstSet { ascii: SPACE, non_ascii: false }, i + 2)),
            b'm' | b'M' => None,
            c => Some((FirstSet::byte(escape_byte(c)), i + 2)),
        },
        b'[' => bracket_set(pat, i),
        b'.' => Some((FirstSet::ANY, i + 1)),
        b'(' | b')' | b'|' | b'^' | b'$' | b'*' | b'+' | b'?' | b'{' => None,
        b if b.is_ascii() => Some((FirstSet::byte(b), i + 1)),
        b => Some((FirstSet { ascii: 0, non_ascii: true }, i + utf8_char_len(b))),
    }
}

// Quantifier suffix at pat[i]: (min, max, index past it); max None = unbounded.
fn quantifier(pat: &[u8], i: usize) -> (u32, Option<u32>, usize) {
    let digits = |mut j: usize| {
        let mut v = 0u32;
        while j < pat.len() && pat[j].is_ascii_digit() {
            v = v * 10 + (pat[j] - b'0') as u32;
            j += 1;
        }
        (v, j)
    };
    match pat.get(i) {
        Some(b'*') => (0, None, i + 1),
        Some(b'+') => (1, None, i + 1),
        Some(b'?') => (0, Some(1), i + 1),
        Some(b'{') => {
            let (m, j) = digits(i + 1);
            if pat[j] == b'}' {
                return (m, Some(m), j + 1);
            }
            if pat[j + 1] == b'}' {
                return (m, None, j + 2);
            }
            let (n, k) = digits(j + 1);
            (m, Some(n), k + 1)
        }
        _ => (1, Some(1), i),
    }
}

// For the group opening at pat[i]: the index of its closing ')' and whether
// a capturing group opens inside it.
fn group_close(pat: &[u8], mut i: usize) -> Option<(usize, bool)> {
    let mut depth = 0usize;
    let mut capturing = false;
    while i < pat.len() {
        match pat[i] {
            b'\\' => i += 1,
            b'[' => {
                i = parse_bracket(pat, i)?;
                continue;
            }
            b'(' => {
                if depth > 0 && pat.get(i + 1) != Some(&b'?') {
                    capturing = true;
                }
                depth += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i, capturing));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// The first set of pat[i..end] when it is a non-empty run of unquantified
// single-character atoms (a fixed string); None otherwise.
fn fixed_run_first(pat: &[u8], mut i: usize, end: usize) -> Option<FirstSet> {
    let mut first = None;
    while i < end {
        let (set, next) = single_atom(pat, i)?;
        if next < end && matches!(pat[next], b'*' | b'+' | b'?' | b'{') {
            return None;
        }
        first.get_or_insert(set);
        i = next;
    }
    first
}

// Per-segment state of the capture-determinacy scan.
#[derive(Default)]
struct Segment {
    // First set of the latest variable-count quantifier whose count is not
    // yet known to be forced by the item that follows it.
    pending: Option<FirstSet>,
    // A variable-count quantifier whose count the input does not force.
    free_seen: bool,
    // The segment contains something outside the analysed item grammar.
    opaque: bool,
}

impl Segment {
    fn unforce(&mut self) {
        if self.pending.take().is_some() {
            if self.free_seen {
                self.opaque = true;
            }
            self.free_seen = true;
        }
    }

    // A consuming item with first set `first` and quantifier (min, max).
    fn item(&mut self, first: FirstSet, min: u32, max: Option<u32>) {
        if max == Some(0) {
            self.unforce();
            return;
        }
        if let Some(prev) = self.pending {
            if min >= 1 && prev.disjoint(first) {
                self.pending = None;
            } else {
                self.unforce();
            }
        }
        if max != Some(min) {
            if self.free_seen {
                self.opaque = true;
            }
            self.pending = Some(first);
        }
    }

    // The segment ends (capture boundary): true when its extent is fixed by
    // its single free count.
    fn close(&mut self) -> bool {
        self.unforce();
        let ok = !self.opaque;
        *self = Segment::default();
        ok
    }
}

// Whether the capture positions of a leftmost-longest match are the same
// under Spencer's dissection (each segment between capture boundaries takes,
// left to right, its longest extent: regexec.c ccondissect over the
// concat(prefix, concat(capture, rest)) tree of regcomp.c parseqatom) and
// under Perl priority (RE2, the program tier: each quantifier, left to right,
// takes its largest count). A quantifier X{m,n} followed by an item that
// must consume a character outside X's first set has a count the input
// forces; if every segment except the last has at most one unforced
// quantifier and no variable-count quantifier after it, the segment's
// extent is strictly increasing in that one count, so both orders pick the
// same parse. Only flat patterns qualify: top-level, unquantified,
// non-nested captures; items are single-character atoms and `(?:...)`
// groups of them, each optionally quantified, plus anchors.
fn captures_determined(pat: &[u8]) -> bool {
    let mut seg = Segment::default();
    let mut in_capture = false;
    let mut i = 0usize;
    while i < pat.len() {
        match pat[i] {
            b'^' | b'$' => {
                seg.unforce();
                i += 1;
            }
            b'(' if pat.get(i + 1) == Some(&b'?') => {
                let Some((close, capturing_inside)) = group_close(pat, i) else {
                    return false;
                };
                if capturing_inside {
                    return false;
                }
                let (min, max, next) = quantifier(pat, close + 1);
                match fixed_run_first(pat, i + 3, close) {
                    Some(first) => seg.item(first, min, max),
                    None => seg.opaque = true,
                }
                i = next;
            }
            b'(' => {
                if in_capture || !seg.close() {
                    return false;
                }
                in_capture = true;
                i += 1;
            }
            b')' => {
                if !in_capture || !seg.close() {
                    return false;
                }
                in_capture = false;
                i += 1;
                if matches!(pat.get(i), Some(b'*' | b'+' | b'?' | b'{')) {
                    return false;
                }
            }
            _ => {
                let Some((set, next)) = single_atom(pat, i) else {
                    return false;
                };
                let (min, max, after) = quantifier(pat, next);
                seg.item(set, min, max);
                i = after;
            }
        }
    }
    !in_capture
}

// A trailing `$` not escaped by an odd backslash run.
fn ends_with_anchor(pat: &[u8]) -> bool {
    if pat.last() != Some(&b'$') {
        return false;
    }
    let slashes = pat[..pat.len() - 1].iter().rev().take_while(|&&b| b == b'\\').count();
    slashes % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::regex_spencer::{REG_EXPANDED, REG_ICASE, REG_NEWLINE, REG_NLANCH, REG_NLSTOP};

    fn setup_utf8() {
        let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    }

    fn ok(p: &str) -> bool {
        setup_utf8();
        re2_compatible(p.as_bytes(), REG_ADVANCED)
    }

    #[test]
    fn admits_compatible_class() {
        for p in [
            r"^https?://(?:m\.)?([^/]+)/.*$",
            "",
            "abc",
            "a|b|",
            "(a)(b)(c)",
            "a{2}b{3,}c{4,5}",
            "[abc]",
            "[^abc]",
            "[a-z0-9]",
            "[-a]",
            "[a-]",
            "[a^b]",
            "x*y+z?",
            "^(foo|bar)$",
            r"\.\*\+\(\)",
            r"a\nb\tc",
            "déjà vu",
            "[é]",
            "((a|b)*c)+",
            "a{0,255}",
        ] {
            assert!(ok(p), "should admit {p:?}");
        }
    }

    #[test]
    fn rejects_incompatible_class() {
        for p in [
            r"(a)\1",       // backref
            r"\d+",         // ctype escape
            r"\w",
            r"\bword\b",
            r"\Aabc",
            r"a*?",         // non-greedy
            r"a{1,2}?",
            r"(?=x)",       // lookaround
            r"(?!x)",
            r"(?<=x)",
            r"(?i)abc",     // inline options
            "[[:alpha:]]",  // named class
            "[[=a=]]",
            "[[.a.]]",
            r"[\d]",        // escape inside bracket
            "[]a]",         // POSIX leading-]: RE2 errors
            "[é-z]",        // non-ASCII range endpoint
            "[a-é]",
            "a{256}",       // beyond Spencer DUPMAX
            "a{2,1}",       // malformed bound
            "a{}",
            "a{,2}",
            "{2}",          // nothing to repeat
            "*a",
            "a**",
            "(a",           // unbalanced
            "a)",
            r"a\",          // trailing backslash
        ] {
            assert!(!ok(p), "should reject {p:?}");
        }
    }

    // D3121: \s, and \m / \M where the adjacent item is all word characters.
    #[test]
    fn admits_translated_escapes_where_exact() {
        setup_utf8();
        let c = |p: &str| classify(p.as_bytes(), REG_ADVANCED);
        for p in [
            r"\s+",
            r"\s",
            r"a\sb",
            r"[^a-z0-9 ]+\s*",
            r"\m(inc|llc|l l c|ltd|co|corp|corporation)\M",
            r"\minc\M",
            r"\m[a-z]+",
            r"[0-9]+\M",
            r"\m(?:ab|c_)+\M x",
            r"x \m(a(b|c)|d)",
            r"(foo\M|\mbar)",
            r"^\s*\mword\M\s*$",
        ] {
            let k = c(p);
            assert_ne!(k.tier, Compat::Incompatible, "should admit {p:?}");
            assert!(k.ascii_subject, "{p:?} runs RE2 only over ASCII subjects");
        }
        assert!(!c("abc").ascii_subject);
        assert!(!c(r"a\.b").ascii_subject);
        for p in [
            r"\m",            // nothing after
            r"\M",            // nothing before
            r"\m\M",
            r"\m.a",          // '.' may be a non-word character
            r"\m[a-z ]",      // the class holds a space
            r"\mé",           // non-ASCII
            r"\m(a|b c|)",    // an empty branch
            r"\m(a| b)",
            r"\ma?",          // possibly empty
            r"\m(ab)*c",
            r"\m\sa",
            r"\m^a",
            r"a?\M",
            r"(a|b )\M",
            r"\s\M",
            r"$\M",
            r"\m*a",          // quantified constraint
            r"a\M+",
            r"\S",            // still off-list
            r"\y",
            r"[\s]",          // escape inside a bracket
        ] {
            assert_eq!(c(p).tier, Compat::Incompatible, "should reject {p:?}");
        }
    }

    #[test]
    fn re2_pattern_translates_only_the_escapes() {
        assert_eq!(re2_pattern(br"\s+"), br"[\x09-\x0D\x20]+".to_vec());
        assert_eq!(re2_pattern(br"\m(inc|co)\M"), br"\b(inc|co)\b".to_vec());
        assert_eq!(re2_pattern(r"a\.\n[^s m]é".as_bytes()), r"a\.\n[^s m]é".as_bytes().to_vec());
    }

    #[test]
    fn leading_zero_bounds_fall_back_to_spencer() {
        for p in ["a{01}", "a{00}", "a{001}", "a{0,01}", "a{1,01}", "a{01,2}", "a{01,}"] {
            assert!(!ok(p), "should reject {p:?}");
        }
        for p in ["a{0}", "a{0,1}", "a{1,2}", "a{0,}", "a{10}"] {
            assert!(ok(p), "should admit {p:?}");
        }
    }

    #[test]
    fn rejects_incompatible_flags() {
        setup_utf8();
        let p = b"abc";
        assert!(re2_compatible(p, REG_ADVANCED));
        assert!(re2_compatible(p, REG_ADVANCED | REG_NOSUB));
        assert!(re2_compatible(p, REG_QUOTE));
        assert!(re2_compatible(p, REG_QUOTE | REG_NOSUB));
        for f in [
            REG_ADVANCED | REG_ICASE,
            REG_ADVANCED | REG_EXPANDED,
            REG_ADVANCED | REG_NLSTOP,
            REG_ADVANCED | REG_NLANCH,
            REG_ADVANCED | REG_NEWLINE,
            REG_QUOTE | REG_ICASE,
            0, // basic
            1, // extended
        ] {
            assert!(!re2_compatible(p, f), "should reject cflags {f:o}");
        }
    }

    #[test]
    fn rejects_non_utf8_pattern() {
        setup_utf8();
        assert!(!re2_compatible(b"a\xffb", REG_ADVANCED));
    }

    #[test]
    fn rejects_nul_bearing_pattern() {
        setup_utf8();
        assert!(!re2_compatible(b"a\x00b", REG_ADVANCED));
        assert!(!re2_compatible(b"\x00", REG_ADVANCED));
        assert!(!re2_compatible(b"a\x00b", REG_QUOTE));
    }

    #[test]
    fn capture_safety_tiers() {
        setup_utf8();
        let tier = |p: &str| classify(p.as_bytes(), REG_ADVANCED).tier;
        // Unquantified captures (the anchored URL-capture shape) stay capture-safe.
        assert_eq!(tier(r"^https?://(?:m\.)?([^/]+)/.*$"), Compat::CaptureSafe);
        assert_eq!(tier("(a)(b)(c)"), Compat::CaptureSafe);
        // Quantified non-capturing subtrees stay capture-safe.
        assert_eq!(tier("(?:ab)+(c)"), Compat::CaptureSafe);
        // Alternation without captures stays capture-safe (nothing to
        // misreport); alternation WITH captures is whole-match only.
        assert_eq!(tier("a|ab|abc"), Compat::CaptureSafe);
        assert_eq!(tier("^(foo|bar)$"), Compat::WholeMatch);
        assert_eq!(tier("(a)|b"), Compat::WholeMatch);
        assert_eq!(tier("(é|.[^a])"), Compat::WholeMatch);
        // A quantifier over a capture-bearing subtree: whole-match only
        // (the adversarial corpus found last-iteration capture divergence).
        assert_eq!(tier("(a)+"), Compat::WholeMatch);
        assert_eq!(tier("(a)?b"), Compat::WholeMatch);
        assert_eq!(tier("(a|b){2,3}"), Compat::WholeMatch);
        assert_eq!(tier("(?:(a)b)+"), Compat::WholeMatch);
        assert_eq!(tier("((a)b)*c"), Compat::WholeMatch);
        assert_eq!(
            tier(r"(.?|é|(?:0{2,}|é[^a][^/,]+ ?|\*)+é(?:c?0|\n{2})+)+|[a-c0-9]?|[ab]0+"),
            Compat::WholeMatch
        );
        // Quoted literals have no captures.
        assert_eq!(classify(b"a.c", REG_QUOTE).tier, Compat::CaptureSafe);
    }

    #[test]
    fn capture_extent_with_competing_quantifiers() {
        setup_utf8();
        for pattern in [
            "(a?(?:ab)?)b?",
            "^(a?(?:ab)?)b?$",
            "(a{0,1}(?:ab){0,1})b?",
            "((?:a?)(?:ab)?)b?",
            "(a*(?:ab)*)b?",
            "(a+(?:ab)*)b?",
        ] {
            assert_eq!(classify(pattern.as_bytes(), REG_ADVANCED).tier, Compat::WholeMatch);
        }
        assert_eq!(classify(b"(a*)b?", REG_ADVANCED).tier, Compat::CaptureSafe);
        assert_eq!(classify(b"(?:a?(?:ab)?)b?", REG_ADVANCED).tier, Compat::CaptureSafe);
    }

    #[test]
    fn capture_segments_need_a_single_unforced_count() {
        setup_utf8();
        let tier = |p: &str| classify(p.as_bytes(), REG_ADVANCED).tier;
        // Segment before the capture has two unforced quantifiers: Spencer
        // gives the prefix its longest extent, Perl priority its first
        // quantifier (audit fp-adt-regexp#1).
        for p in [
            "a?(?:ab)?(b?)",
            "^a?(?:ab)?(b?)$",
            "a*(?:ab)?(b?)",
            "(?:(a*)(?:ab)?)(b?)",
            "a*b?(c)",
            "(a?b?)",
            "(?:a*b)?(c)",
            "(a*b?)c",
        ] {
            assert_eq!(tier(p), Compat::WholeMatch, "{p:?}");
        }
        // One unforced count per segment, or counts forced by a disjoint
        // mandatory successor.
        for p in [
            r"^https?://(?:m\.)?([^/]+)/.*$",
            "a?(?:ab)?b?",
            "(a*)b?",
            "a*(b)",
            "a*b(c+)d?e*",
            "(a*b+)c",
            "x*y(z+)",
            "(a*).*(b)",
            "[ab]*c{2,3}(d)",
            r"(\.*)x",
            "a{2}(b{3})",
            "(é+)a",
            "x(a*)b*(c)",
        ] {
            assert_eq!(tier(p), Compat::CaptureSafe, "{p:?}");
        }
    }

    #[test]
    fn longest_mode_for_alternation_and_unpinned_quantifiers() {
        setup_utf8();
        let c = |p: &str| classify(p.as_bytes(), REG_ADVANCED);
        assert!(!c(r"^https?://(?:m\.)?([^/]+)/.*$").needs_longest);
        assert!(!c("^a*b+c{2,3}$").needs_longest);
        assert!(!c("abc").needs_longest);
        assert!(!c("^abc").needs_longest);
        assert!(c("a*b+c{2,3}").needs_longest);
        assert!(c("a*(?:ab)?").needs_longest);
        assert!(c("^a*(?:ab)?").needs_longest);
        assert!(c("a*(?:ab)?$").needs_longest);
        assert!(c(r"^a*\$").needs_longest);
        assert!(!c(r"^a*\\$").needs_longest);
        assert!(c("a|ab").needs_longest);
        assert!(c("(?:a|b)c").needs_longest);
        assert!(!classify(b"a|b", REG_QUOTE).needs_longest);
        // D2778 (trunk): first-match can end short of Spencer's longest match with two variable quantifiers
        // (`^(?:ab)?(?:abcd)?` over `abcd`); a pattern pinned at both ends cannot; a fixed bound is not variable.
        assert!(c("^(?:ab)?(?:abcd)?").needs_longest);
        assert!(c("^https?://(?:m\\.)?").needs_longest);
        assert!(!c("^(?:ab)?(?:abcd)?$").needs_longest);
        assert!(c("a{2,}b?").needs_longest);
        assert_eq!(c("^(?:ab)?(abcd)?").tier, Compat::WholeMatch);
        assert_eq!(c("^https?://(?:m\\.)?([^/]+)/.*$").tier, Compat::CaptureSafe);
    }

    #[test]
    fn rejects_beyond_complexity_budget() {
        setup_utf8();
        // The regex regress suite's Spencer-ETOOBIG case: RE2 compiles it,
        // Spencer errors — must never be admitted.
        assert!(!ok(&"x*y*z*".repeat(1000)));
        // Length budget (also applies to quoted literals: Spencer states
        // scale with literal length).
        assert!(!ok(&"a".repeat(MAX_PATTERN_BYTES + 1)));
        assert!(ok(&"a".repeat(MAX_PATTERN_BYTES)));
        assert!(!re2_compatible("a".repeat(MAX_PATTERN_BYTES + 1).as_bytes(), REG_QUOTE));
        // Quantifier budget.
        assert!(!ok(&"x*".repeat(MAX_QUANTIFIERS as usize + 1)));
        assert!(ok(&"x*".repeat(MAX_QUANTIFIERS as usize)));
    }
}
