//! The dictionary path's regexp (`regexp::compile` + `replace_first`) against PostgreSQL's own engine (Spencer, forced,
//! through `textregexreplace_noopt`, the fmgr's replacement routine): byte-identical output over structured patterns
//! (anchored and unanchored, up to nine groups, `.` and classes under every quantifier) x a URL-like corpus with empty,
//! non-ASCII, scheme-less, slash-less and newline subjects; and `required_literal` against PostgreSQL's match verdicts.
mod common;

use common::{url_corpus, Buf};
use gravity_dict::regexp::{compile, compile_under, flags_form, replace_all, replace_first, required_literal, FlagsForm, Outcome, Template};

/// The session GUCs are thread-local: armed on every test thread, PostgreSQL's engine forced.
fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        regex_core::init_seams();
        adt_regexp::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
    });
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
    regexp_alt::set_regex_engine(guc_tables::consts::REGEX_ENGINE_SPENCER);
}

fn pg_replace(pattern: &[u8], subject: &[u8], replacement: &[u8]) -> Vec<u8> {
    setup();
    let cx = mcx::MemoryContext::new("regexp_corpus");
    let out = adt_regexp::textregexreplace_noopt(cx.mcx(), subject, pattern, replacement, types_core::C_COLLATION_OID).unwrap().as_slice().to_vec();
    out
}

/// `regexp_replace(subject, pattern, replacement, flags)` through PostgreSQL's engine; `Err` its error's SQLSTATE and text.
fn pg_replace_flags(pattern: &[u8], subject: &[u8], replacement: &[u8], flags: &[u8]) -> Result<Vec<u8>, String> {
    setup();
    let cx = mcx::MemoryContext::new("regexp_corpus");
    let out = adt_regexp::textregexreplace(cx.mcx(), subject, pattern, replacement, flags, types_core::C_COLLATION_OID).map(|v| v.as_slice().to_vec()).map_err(|e| format!("{:?} {}", e.sqlstate(), e.message()));
    out
}

/// PostgreSQL's engine finds a match (a zero-length one too) in `subject`: the marker byte, in no subject, appears.
fn pg_matches(pattern: &[u8], subject: &[u8]) -> bool {
    pg_replace(pattern, subject, b"\x01") != subject
}

fn subjects() -> Vec<Vec<u8>> {
    let mut s = url_corpus(4000, 7);
    for extra in ["", "/", "http://", "https://", "http:///x", "http://m./x", "http://m.", "http://m.a", "http://m.a/", "http:/a/b", "https://a", "https://a/", "hello", "http://пример.рф/страница", "http://m.пример.рф/", "https://日本.jp/index", "ftp://x/y", "HTTP://A/B", "http://a/b\nc", "http://a\n/b", "\n", "http://x//", "m.a/b", "http://ab.cd", "http://a.b.c/d.e", "aaa.bbb.ccc", "a.", "http://x.y.", "abc-def_ghi", "12345", "x1y2z3", "http://host:8080/p?q=1#f", "xhttp://a/b", "http://m.m.a/b", "a.b.com", "a.com.com/x", "x.comz", ".com", "http://a.b?c=d", "https://m.a.b#c", "http://\nhost/x", "http://host\n", "http://m.a\n", "http://a.b\n/c\n", "https://a\nb", "a.b.com\n", "abc\n"] {
        s.push(extra.as_bytes().to_vec());
    }
    s
}

const SHAPES: &[(&str, &str)] = &[
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"\1"),
    (r"^http://([^.]+)\..*$", r"\1"),
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"host=\1"),
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"\&"),
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"\2"),
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"\\\1\"),
    (r"^https?://(?:m\.)?([^/]+)/.*$", r"[\1]\x"),
    (r"^https?://(?:m\.)?([^/]+)/", r"\1"),
    (r"^https?://(?:m\.)?([^/]+)", r"<\1>"),
    (r"^http://", ""),
    (r"^", "pre:"),
    (r"^$", "empty"),
    (r"^(?:m\.)?([a-z0-9.-]+)$", r"\1"),
    (r"^[a-z]+", "W"),
    (r"^x*y+z?", "Q"),
    (r"^([0-9]{4})-", r"\1"),
    (r"^http://([^/]+)/([^/]+)", r"\1"),
    (r"^h(t)tp", r"\1"),
    (r"^a.*$", "A"),
    (r"^(?:http|https)://", ""),
    // a class run whose next literal starts inside the class: not possessive, the capture ends at the last `.com`
    (r"^([a-z.]+)\.com.*$", r"\1"),
    // a class that admits '\n' before `$`: possessive, as `$` (no newline-sensitive flag in the subset) is the end alone
    (r"^https?://(?:m\.)?([^/]+)$", r"\1"),
    // the widened subset (WIDENED): unanchored, two or more groups, `.` anywhere
    (r"https?://(?:m\.)?([^/]+)/.*$", r"\1"),
    ("://([^/]+)/", r"[\1]"),
    (r"^https?://(?:m\.)?([^/.]+)\.([^/]+)/.*$", r"\2.\1"),
    ("^/([^/]+)/([^/]+)/.*$", r"\2/\1"),
    ("^(.*)/(.*)$", r"\2 \1"),
    ("^(.)(.)(.)(.)", r"\4\3\2\1"),
    ("a.b", "X"),
    ("[0-9]+", "#"),
    ("/([^/]+)/[0-9]+", r"\1"),
    ("([A-Za-z]+)/v?[0-9.]+", r"\1"),
    (r"([a-z]+)@([a-z]+)\.", r"\2@\1."),
    (r"\.([a-z]+)/", r"<\1>"),
    (r"([a-z]+)\.([a-z]+)", r"\2.\1"),
    ("([^/]{2,3})x", r"\1"),
];

/// The shapes past the old anchored one-group subset that run per entry.
const WIDENED: &[&str] = &[r"https?://(?:m\.)?([^/]+)/.*$", "://([^/]+)/", r"^https?://(?:m\.)?([^/.]+)\.([^/]+)/.*$", "^/([^/]+)/([^/]+)/.*$", "^(.*)/(.*)$", "^(.)(.)(.)(.)", "a.b", "[0-9]+", "/([^/]+)/[0-9]+", "([A-Za-z]+)/v?[0-9.]+", r"([a-z]+)@([a-z]+)\.", "([^/]{2,3})x"];

#[test]
fn programs_replace_exactly_as_postgresql() {
    let subjects = subjects();
    let mut buf = Buf::new(1 << 16);
    let (mut compiled, mut refused, mut pairs) = (Vec::new(), Vec::new(), 0);
    for (p, r) in SHAPES {
        let (Some(prog), Some(tmpl)) = (compile(p.as_bytes()), Template::parse(r.as_bytes())) else {
            refused.push(*p);
            continue;
        };
        compiled.push(*p);
        for s in &subjects {
            let mut region = buf.region();
            let got = replace_first(&prog, &tmpl, r.as_bytes(), s, &mut region).unwrap().unwrap_or_else(|| panic!("{p:?}: the corpus runs within the step budget"));
            let want = pg_replace(p.as_bytes(), s, r.as_bytes());
            assert_eq!(unsafe { got.bytes() }, &want[..], "{p:?} -> {r:?} on {:?}", String::from_utf8_lossy(s));
            let in_place = tmpl.single_group() && prog.anchored() && prog.anchored_to_end();
            if in_place && matches!(prog.find(s, 0), Outcome::Match(_)) {
                assert!(s.as_ptr_range().contains(&got.ptr) || got.len == 0, "{p:?}: an in-place result lies in the subject");
            }
            pairs += 1;
        }
    }
    println!("{} shapes compiled, refused {refused:?}, {pairs} pairs", compiled.len());
    for p in WIDENED {
        assert!(compiled.contains(p), "{p:?} runs per entry");
    }
    assert!(refused.contains(&r"^(?:http|https)://"), "an alternation stays on the function manager");
}

/// A-16b: the shapes again under `(?i)` (`regexp_replace`'s flag `'i'` as lowerv4's derived key spells it), through
/// `compile_under` for a collation whose case map is ASCII's, against PostgreSQL's engine on the same pattern under the C
/// collation, over the corpus and its case-flipped copy, plus shapes whose letters sit in classes and negated classes and
/// subjects with the non-ASCII characters some case maps tie to ASCII letters (the Kelvin sign, long s, dotted / dotless i).
#[test]
fn folded_programs_replace_exactly_as_postgresql() {
    let mut subjects = subjects();
    let flip = |s: &Vec<u8>| s.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect::<Vec<u8>>();
    let flipped: Vec<Vec<u8>> = subjects.iter().map(flip).collect();
    subjects.extend(flipped);
    for extra in ["HTTPS://WWW.Example.COM/Path", "Http://Www.A/b", "hTtP://x/Y", "K\u{212a}k", "\u{17f}sS", "\u{130}i\u{131}I", "ABC-def"] {
        subjects.push(extra.as_bytes().to_vec());
    }
    let folded: &[(&str, &str)] = &[("[^a-c]+", "#"), (r"^(?:WWW\.)?([A-Z]+)", r"\1"), ("[K]", "x"), ("k", "x"), ("k+", "x"), ("s+", "x"), ("i", "x"), ("[h-j]{2}", "x"), ("([A-Za-z]+)-([a-z]+)", r"\2-\1")];
    let mut buf = Buf::new(1 << 16);
    let (mut compiled, mut pairs) = (Vec::new(), 0);
    for (p, r) in SHAPES.iter().chain(folded) {
        let pat = [b"(?i)", p.as_bytes()].concat();
        let (Some(prog), Some(tmpl)) = (compile_under(&pat, || true), Template::parse(r.as_bytes())) else { continue };
        compiled.push(*p);
        for s in &subjects {
            let mut region = buf.region();
            let got = replace_first(&prog, &tmpl, r.as_bytes(), s, &mut region).unwrap().unwrap_or_else(|| panic!("{p:?}: the corpus runs within the step budget"));
            let want = pg_replace(&pat, s, r.as_bytes());
            assert_eq!(unsafe { got.bytes() }, &want[..], "(?i){p:?} -> {r:?} on {:?}", String::from_utf8_lossy(s));
            pairs += 1;
        }
    }
    println!("{} folded shapes compiled, {pairs} pairs", compiled.len());
    // `^(?:WWW\.)?([A-Z]+)` stays on the function manager folded or not: `[A-Z]+` admits `w`, so the optional `WWW.` is
    // a choice point PostgreSQL's longest match can leave (D2778)
    for p in WIDENED.iter().chain(&[r"^https?://(?:m\.)?([^/]+)/.*$", "[^a-c]+", "k", "k+", "s+", "[h-j]{2}"]) {
        assert!(compiled.contains(p), "(?i){p:?} runs per entry");
    }
    assert!(compile_under(b"(?i)abc", || false).is_none(), "a collation whose case map leaves ASCII keeps the function manager");
    assert!(compile_under("(?i)é".as_bytes(), || true).is_none(), "a non-ASCII pattern letter's cases are the collation's");
    assert!(compile_under(b"(?i)(?n)abc", || true).is_none() && compile_under(b"^(?i)abc", || true).is_none(), "options only lead");
}

/// The shapes `'g'` adds: matches that can be empty (at a start, between characters, at the end), several and adjacent
/// matches, candidates that overlap (the next search starts at the previous match's end), and multibyte characters, which
/// an empty match steps over whole.
const GLOBAL_SHAPES: &[(&str, &str)] = &[
    ("[0-9]*", "N"),
    ("[0-9]+", "N"),
    ("[a-z]*", r"<\&>"),
    (".*", "X"),
    (".", r"\&\&"),
    ("[^/]*", "_"),
    ("[^a]", "-"),
    ("b?", "B"),
    ("(?:ab)?", "|"),
    ("$", "END"),
    ("[0-9]*$", "#"),
    ("", "^"),
    ("aa", "X"),
    ("aba", "X"),
    ("a.a", r"[\&]"),
    ("[ab]{2}", "X"),
    ("([a-z])([0-9])", r"\2\1"),
    ("/([^/]*)", r"(\1)"),
    ("\\.", ""),
    ("w{2,3}", "W"),
    ("[.]", r"\\"),
    ("(о)", r"\1\1"),
    ("ü*", "U"),
];

/// `'g'` subjects: the URL corpus plus the boundary cases (empty, one character, a match at the end, repeated and
/// overlapping candidates, multibyte runs, newlines).
fn global_subjects() -> Vec<Vec<u8>> {
    let mut s = subjects();
    for extra in ["a", "aa", "aaa", "aaaa", "abab", "ababa", "abba", "ab1cd2", "a1b2c3", "123", "1a", "a1", "w", "www.www.w", "wwwww", "x/y/", "//", "/", "aüüb", "üüü", "ü", "ööo", "оооо", "日本語", "a\nb", "a\n", "\n\n", "a.b..c...", "b", "bbb", "abc1", "....", "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t"] {
        s.push(extra.as_bytes().to_vec());
    }
    s
}

/// The replace-all loop (`replace_all`, flag `'g'`) against PostgreSQL's engine with flags `'g'`: byte-identical over the
/// corpus shapes and the `'g'` shapes (empty matches, matches at the end, overlapping candidates, multibyte), plain and
/// under `(?i)` (`compile_under`, a collation whose case map is ASCII's) over the case-flipped corpus too.
#[test]
fn every_match_replaces_exactly_as_postgresql() {
    let mut subjects = global_subjects();
    let flip = |s: &Vec<u8>| s.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect::<Vec<u8>>();
    let flipped: Vec<Vec<u8>> = subjects.iter().map(flip).collect();
    subjects.extend(flipped);
    let mut buf = Buf::new(1 << 20);
    let (mut compiled, mut refused, mut pairs) = (Vec::new(), Vec::new(), 0);
    for (p, r) in SHAPES.iter().chain(GLOBAL_SHAPES) {
        for icase in [false, true] {
            let pat = if icase { [b"(?i)", p.as_bytes()].concat() } else { p.as_bytes().to_vec() };
            let (Some(prog), Some(tmpl)) = (compile_under(&pat, || true), Template::parse(r.as_bytes())) else {
                refused.push((*p, icase));
                continue;
            };
            compiled.push((*p, icase));
            for s in &subjects {
                let mut region = buf.region();
                let got = replace_all(&prog, &tmpl, r.as_bytes(), s, &mut region).unwrap().unwrap_or_else(|| panic!("{p:?}: the corpus runs within the step budget"));
                let want = pg_replace_flags(&pat, s, r.as_bytes(), b"g").unwrap();
                assert_eq!(unsafe { got.bytes() }, &want[..], "{:?} -> {r:?} 'g' on {:?}", String::from_utf8_lossy(&pat), String::from_utf8_lossy(s));
                pairs += 1;
            }
        }
    }
    println!("{} shape forms compiled, refused {refused:?}, {pairs} pairs", compiled.len());
    for p in ["[0-9]*", "[0-9]+", ".*", ".", "b?", "$", "", "aa", "aba", "[ab]{2}", "([a-z])([0-9])", "(о)", r"https?://(?:m\.)?([^/]+)/.*$", "://([^/]+)/"] {
        assert!(compiled.contains(&(p, false)), "{p:?} runs per entry under 'g'");
    }
    assert!(pairs > 200_000, "{pairs} pairs");
}

/// What `flags_form` keeps of a flags string answers as PostgreSQL's engine does under the whole string: for every pattern
/// and flags string it accepts, `regexp_replace(s, p, r, flags)` equals the call under the canonical flags (`'g'` and
/// `'i'` alone) over subjects with newlines, white space, `#` and multibyte text; every letter PostgreSQL rejects is
/// rejected (the call raises 22023).
#[test]
fn the_flags_form_answers_as_the_whole_flags() {
    let patterns = ["[0-9]+", "a b", "a#b", "[a-z]+", "^[a-z]+", "[a-z]+$", "a.b", "[^x]+", "\\D+", "\\d+", "\\S+", "\\w+", "\\W", "x", "(?x)a b", "(?s)[a-z]+", "\\.", "[[:alpha:]]+", "ab|cd", "(a)(b)?"];
    let flags = ["", "g", "i", "c", "s", "t", "n", "m", "p", "w", "x", "gi", "ic", "ci", "gn", "gns", "gsn", "gp", "gw", "gpw", "gwp", "gx", "gxt", "gtx", "gic", "gnsxti", "q", "b", "e", "gq", "gb", "ge", "qb", "be", "z", "1", "g1", "G", "gI", "é"];
    let subjects: Vec<&[u8]> = vec![b"", b"abc", b"a b", b"a#b", b"ab\ncd\n", b"\n", b"x1 y2\nz3", b"A\nB c#d", "über\nötz 9".as_bytes(), b"a.b\na\nb", b"ab  cd\tef", b"#"];
    let mut accepted = 0;
    for p in patterns {
        for f in flags {
            let whole = subjects.iter().map(|s| pg_replace_flags(p.as_bytes(), s, b"<\\&>", f.as_bytes())).collect::<Vec<_>>();
            match flags_form(f.as_bytes(), p.as_bytes()) {
                Some(FlagsForm { global, icase }) => {
                    accepted += 1;
                    let canon = format!("{}{}", if global { "g" } else { "" }, if icase { "i" } else { "" });
                    for (s, w) in subjects.iter().zip(&whole) {
                        let c = pg_replace_flags(p.as_bytes(), s, b"<\\&>", canon.as_bytes());
                        assert_eq!(&c, w, "{p:?} flags {f:?} as {canon:?} on {:?}", String::from_utf8_lossy(s));
                    }
                }
                None => {
                    if f.bytes().any(|c| !b"gbceimnpqstwx".contains(&c)) {
                        assert!(whole.iter().all(|w| w.as_ref().is_err_and(|e| e.contains("invalid regular expression option"))), "{f:?}: PostgreSQL rejects it");
                    }
                }
            }
        }
    }
    println!("{accepted} accepted (pattern, flags) pairs");
    for (f, p) in [("g", "[0-9]+"), ("gn", "[0-9]+"), ("gx", "[0-9]+"), ("gp", "\\d+"), ("gnsxti", "a.b"), ("gw", "\\.")] {
        assert!(flags_form(f.as_bytes(), p.as_bytes()).is_some(), "{f:?} over {p:?} takes the program");
    }
    for (f, p) in [("gn", "a.b"), ("gw", "^[a-z]+"), ("gp", "[^x]+"), ("n", "\\D+"), ("gx", "a b"), ("x", "a#b"), ("q", "x"), ("b", "x"), ("e", "x"), ("G", "x")] {
        assert!(flags_form(f.as_bytes(), p.as_bytes()).is_none(), "{f:?} over {p:?} keeps the call");
    }
}

#[test]
fn outside_the_subset_is_refused() {
    for p in ["^a|b", "a|ab", "(a)+", "^(a)?", "^((a))", "^[é]+", "^(?i)abc", "^a{300}", r"^\1", "(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)", "(?n)^https?://([^/]+)$", "^(?n)https?://([^/]+)$", "(?w)^a$", "***:^a$"] {
        assert!(compile(p.as_bytes()).is_none(), "{p:?} compiles");
    }
    // D2778: a choice point with a variable-length op after it, the match not ending at the subject's end (PostgreSQL's
    // longest match is not the greedy order's first); the same shapes anchored to the end, or a choice point last, compile
    for p in ["^(?:ab)?(?:abcd)?", "^(?:a)?(?:ab)?", "^(?:m)?(?:m\\.)?", "^a*(?:ab)?", "^https?://(?:m\\.)?([^/]+)"] {
        assert!(compile(p.as_bytes()).is_none(), "{p:?} compiles");
    }
    for p in ["^(?:ab)?(?:abcd)?.*$", "^(?:m)?(?:m\\.)?$", "^https?://(?:m\\.)?", "^https?://(?:m\\.)?([^/]+)/.*$", "^[a-z]*b", "abc", "^a.b", "^.*a", "^(a)(b)", "^[^/]", "^[^/]{2,3}"] {
        assert!(compile(p.as_bytes()).is_some(), "{p:?} is refused");
    }
}

#[test]
fn the_required_literal_is_in_every_match() {
    let subjects = subjects();
    let cases: &[(&str, Option<&str>)] = &[
        (r"^https?://(?:m\.)?([^/]+)/.*$", Some("http")),
        (r"^http://([^.]+)\..*$", Some("http://")),
        (r"example\.com", Some("example")),
        (r"a|b", None),
        (r"(?i)http", None),
        (r"x*yz+w", Some("yz")),
        (r"ab{0,2}c", Some("a")),
        (r"ab{2}c", Some("ab")),
        (r"[abc]+ftp", Some("ftp")),
        (r"a{2,3}bcd", Some("bcd")),
        (r"(ab)cd(ef)?", Some("cd")),
        (r"é?xyz", Some("xyz")),
        (r"index\.html$", Some("index")),
        (r"\.ru/", Some("ru/")),
        (r"[[:alpha:]]+/item/", Some("/item/")),
        (r"^.*post-", Some("post-")),
        (r"***:abc", None),
        (r"", None),
        (r"a.b", Some("a")),
        // D3570: an escape past two bytes is one character (`\u00a0`, `\x41`, `\U..`, octal, `\c`) or a back
        // reference: no literal; a two-byte escape ends the run; `\]` inside a bracket is a member, not its end
        (r"\u00a0", None),
        (r"New\u00a0York", None),
        (r"ab\x41cde", None),
        (r"\U000000a0xyz", None),
        (r"(ab)cde\1", None),
        (r"abc\0def", None),
        (r"abc\", None),
        (r"x\dyz", Some("yz")),
        (r"[-_\]]v[0-9]+", Some("v")),
        (r"[\]]xyz", Some("xyz")),
        (r"[^\]a]+bcd", Some("bcd")),
    ];
    for (p, want) in cases {
        let got = required_literal(p.as_bytes()).map(|(s, e)| &p.as_bytes()[s..e]);
        assert_eq!(got, want.map(str::as_bytes), "{p:?}");
        let Some(lit) = got else { continue };
        for s in &subjects {
            if pg_matches(p.as_bytes(), s) {
                assert!(s.windows(lit.len()).any(|w| w == lit), "{p:?} matches {:?} without {:?}", String::from_utf8_lossy(s), String::from_utf8_lossy(lit));
            }
        }
    }
}

/// `replace_first_memo` answers as `replace_first` does over every shape, whatever order the
/// subjects come in: sorted (a dictionary's — the memo's case), reversed, and a mix built to
/// break a prefix memo (a subject that is a prefix of the last, one that extends a scan to the
/// end, one that turns an optional literal on or off, the empty subject in between).
#[test]
fn the_prefix_memo_answers_as_the_program() {
    use gravity_dict::regexp::{replace_first_memo, Memo};
    let mut subjects = subjects();
    for extra in ["http://a/", "http://a/x", "http://a", "http://a/x/y", "http://m.a/", "http://m.a/b", "http://m.a", "http://m.ab/", "http://ab/", "http://a.b/", "http://a.bc/", "http://a.", "http://a.b", "aaa", "aaaa", "aa-", "aaa-", "aaa-b", "2024-", "2024-x", "20241", "http://host/a", "http://host/b", "http://host:1/c", "http://host", "http://hos", "https://host/a", "https://host/", "xyz", "xyzz", "xy"] {
        subjects.push(extra.as_bytes().to_vec());
    }
    let mut sorted = subjects.clone();
    sorted.sort();
    let mut reversed = sorted.clone();
    reversed.reverse();
    let mut mixed = Vec::new();
    for (i, s) in sorted.iter().enumerate() {
        mixed.push(s.clone());
        if i % 3 == 0 {
            mixed.push(Vec::new());
        }
        if i % 5 == 0 && s.len() > 2 {
            mixed.push(s[..s.len() - 1].to_vec());
            mixed.push(s.clone());
        }
    }
    let mut buf = Buf::new(1 << 16);
    let mut pairs = 0;
    for shapes in [SHAPES, &[(r"^([a-z]+).*$", r"\1"), (r"^(?:m\.)?([^/]+)/.*$", r"\1"), (r"^([0-9]{4})-.*$", r"\1"), (r"^(x*)y+.*$", r"\1"), (r"^https?://(?:m\.)?([^/]+)/.*", r"\1")]] {
        for (p, r) in shapes {
            let (Some(prog), Some(tmpl)) = (compile(p.as_bytes()), Template::parse(r.as_bytes())) else { continue };
            for order in [&sorted, &reversed, &mixed] {
                let mut memo = Memo::new();
                for s in order.iter() {
                    let mut region = buf.region();
                    let want = replace_first(&prog, &tmpl, r.as_bytes(), s, &mut region).unwrap().expect("no refusal");
                    let want = unsafe { want.bytes() }.to_vec();
                    let mut region = buf.region();
                    let got = replace_first_memo(&prog, &tmpl, r.as_bytes(), s, &mut region, &mut memo).unwrap().expect("no refusal");
                    assert_eq!(unsafe { got.bytes() }, &want[..], "{p:?} → {r:?} on {:?}", String::from_utf8_lossy(s));
                    pairs += 1;
                }
            }
        }
    }
    println!("{pairs} pairs");
    assert!(pairs > 100_000);
}

/// A dictionary's worth of one host's URLs in byte order answers from the memo: the program runs
/// once per host, the rest compare a prefix.
#[test]
fn the_prefix_memo_hits_across_a_hosts_entries() {
    use gravity_dict::regexp::{replace_first_memo, Memo};
    let (prog, tmpl) = (compile(br"^https?://(?:m\.)?([^/]+)/.*$").unwrap(), Template::parse(br"\1").unwrap());
    let mut entries: Vec<Vec<u8>> = Vec::new();
    for host in ["shop.example.org", "m.video.net", "news.site.io", "docs.host.dev"] {
        for i in 0..5000 {
            entries.push(format!("http://{host}/search?text={i}&page={}", i % 7).into_bytes());
        }
    }
    entries.sort();
    let mut buf = Buf::new(1 << 16);
    let mut memo = Memo::new();
    for e in &entries {
        let mut region = buf.region();
        let got = replace_first_memo(&prog, &tmpl, b"\\1", e, &mut region, &mut memo).unwrap().expect("no refusal");
        let rest = &e[b"http://".len()..];
        let host = &rest[..rest.iter().position(|&c| c == b'/').unwrap()];
        let host = host.strip_prefix(b"m.").unwrap_or(host);
        assert_eq!(unsafe { got.bytes() }, host);
    }
    assert!(memo.runs <= 8 && memo.hits + memo.runs == entries.len() as u64, "runs {} hits {}", memo.runs, memo.hits);
}
