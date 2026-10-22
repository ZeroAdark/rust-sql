//! `like::Matcher` against `adt_like` (pgrust's `like_match.c`, the routine `textlike` /
//! `textnlike` / `texticlike` reach under a deterministic collation in a UTF-8 database):
//! byte-identical verdicts and faults over an exhaustive small corpus and a URL-like one.
mod common;

use common::{url_corpus, Buf};
use gravity_dict::like::{Class, FAULT_TRAILING_ESCAPE};
use gravity_dict::{Fault, Matcher, Mode};

fn reference(text: &[u8], pattern: &[u8], mode: Mode) -> Result<bool, Fault> {
    let lower = |b: &[u8]| b.iter().map(u8::to_ascii_lowercase).collect::<Vec<u8>>();
    let r = match mode {
        Mode::Like => adt_like::utf8_match_text(text, pattern, None),
        // generic_text_ic_like under ctype C: both operands ASCII-lowered, then the UTF-8 matcher
        Mode::ILike => adt_like::utf8_match_text(&lower(text), &lower(pattern), None),
    };
    match r {
        Ok(v) => Ok(v == adt_like::LIKE_TRUE),
        Err(e) => {
            assert!(e.message().contains("must not end with escape character"), "{e:?}");
            Err(FAULT_TRAILING_ESCAPE)
        }
    }
}

fn check(texts: &[Vec<u8>], patterns: &[Vec<u8>], mode: Mode) -> (usize, [usize; 7]) {
    let mut buf = Buf::new(1 << 16);
    let mut classes = [0usize; 7];
    let mut n = 0;
    for p in patterns {
        let mut r = buf.region();
        let m = Matcher::new(p, mode, &mut r).unwrap();
        classes[m.class() as usize] += 1;
        let neg = Matcher::new(p, mode, &mut r).unwrap().negated();
        for t in texts {
            let want = reference(t, p, mode);
            let got = m.matches(t);
            assert_eq!(got, want, "{:?} {mode:?} {:?} ({:?})", String::from_utf8_lossy(t), String::from_utf8_lossy(p), m.class());
            assert_eq!(neg.matches(t), want.map(|v| !v), "negated");
            n += 1;
        }
    }
    (n, classes)
}

fn strings(alphabet: &[&str], max: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![Vec::new()];
    let mut layer: Vec<Vec<u8>> = vec![Vec::new()];
    for _ in 0..max {
        let mut next = Vec::new();
        for s in &layer {
            for a in alphabet {
                let mut t = s.clone();
                t.extend_from_slice(a.as_bytes());
                next.push(t);
            }
        }
        out.extend(next.iter().cloned());
        layer = next;
    }
    out
}

#[test]
fn exhaustive_small_alphabets_match_like_match_c() {
    let texts = strings(&["a", "b", "A", "%", "_", "\\", "é", "日"], 4);
    let patterns = strings(&["a", "b", "A", "%", "_", "\\", "é", "日"], 3);
    let (n, classes) = check(&texts, &patterns, Mode::Like);
    println!("LIKE: {n} pairs, classes any/exact/prefix/suffix/contains/segments/general = {classes:?}");
    assert!(classes.iter().all(|&c| c > 0), "every class is exercised: {classes:?}");
    let (n, _) = check(&texts, &patterns, Mode::ILike);
    println!("ILIKE: {n} pairs");
}

#[test]
fn structured_patterns_over_a_url_corpus() {
    let corpus = url_corpus(3000, 11);
    let mut patterns: Vec<Vec<u8>> = ["%garden%", "%Garden%", "http://example.com%", "%.ru/", "%?ref=search", "%/item/%", "http://%.jp%", "%utm\\_source=%", "%\\%%", "http://m.video.net/%", "%example%", "%日本%", "%.рф%", "пример%", "%/blog/____/__/post-%", "%a.b.c%", "_ttp://%", "%=", "http://%/%/%", "%v2%items%", "", "%", "%%", "http://", "%post-1%", "%ITEM%", "%Example.COM%"]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    patterns.push(b"%items\\".to_vec());
    for mode in [Mode::Like, Mode::ILike] {
        let (n, classes) = check(&corpus, &patterns, mode);
        println!("{mode:?}: {n} pairs, classes {classes:?}");
    }
}

#[test]
fn classification_is_what_the_plan_expects() {
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let class = |p: &[u8], r: &mut gravity_dict::Region| Matcher::new(p, Mode::Like, r).unwrap().class();
    assert_eq!(class(b"%", &mut r), Class::Any);
    assert_eq!(class(b"%%%", &mut r), Class::Any);
    assert_eq!(class(b"", &mut r), Class::Exact);
    assert_eq!(class(b"abc", &mut r), Class::Exact);
    assert_eq!(class(b"abc%", &mut r), Class::Prefix);
    assert_eq!(class(b"abc%%", &mut r), Class::Prefix);
    assert_eq!(class(b"%abc", &mut r), Class::Suffix);
    assert_eq!(class(b"%%abc%%", &mut r), Class::Contains);
    assert_eq!(class(b"%a\\%c%", &mut r), Class::Contains);
    assert_eq!(class(b"a%c", &mut r), Class::Segments);
    assert_eq!(class(b"%a%c%", &mut r), Class::Segments);
    assert_eq!(class(b"%%a%%c%%", &mut r), Class::Segments);
    assert_eq!(class(b"%a\\%%c\\_d%", &mut r), Class::Segments, "escaped % and _ are literal bytes of a run");
    assert_eq!(class(b"%a%_c%", &mut r), Class::General);
    assert_eq!(class(b"%a%c\\", &mut r), Class::General);
    assert_eq!(class(b"%a%\xa9%", &mut r), Class::General, "a run starting inside a character is never reachable by the scan");
    assert_eq!(Matcher::new(b"%a%c%", Mode::ILike, &mut r).unwrap().class(), Class::General, "ILIKE keeps the general matcher");
    assert_eq!(class(b"a_c", &mut r), Class::General);
    assert_eq!(class(b"%a_c%", &mut r), Class::General);
    assert_eq!(class(b"abc\\", &mut r), Class::General);
    assert_eq!(class("%é%".as_bytes(), &mut r), Class::Contains);
    assert_eq!(class(b"%\xa9%", &mut r), Class::General, "a literal starting inside a character is never reachable by the scan");
    let m = Matcher::new(b"%a\\%C%", Mode::ILike, &mut r).unwrap();
    assert_eq!(m.literal(), b"a%c");
    assert!(m.matches(b"xxA%Cyy").unwrap() && !m.matches(b"xxAxCyy").unwrap());
    assert_ne!(Matcher::new(b"a%", Mode::Like, &mut r).unwrap().fingerprint(), Matcher::new(b"a%", Mode::ILike, &mut r).unwrap().fingerprint());
    assert_ne!(Matcher::new(b"a%", Mode::Like, &mut r).unwrap().fingerprint(), Matcher::new(b"a%", Mode::Like, &mut r).unwrap().negated().fingerprint());
}

/// D4009: a seeded random corpus beyond the exhaustive one (longer patterns, more `%` groups,
/// escapes, multibyte characters), every class and both modes against `like_match.c`.
#[test]
fn random_patterns_and_texts_match_like_match_c() {
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut word = |alphabet: &[&str], max: u64| -> Vec<u8> {
        let n = next() % (max + 1);
        (0..n).flat_map(|_| alphabet[(next() % alphabet.len() as u64) as usize].bytes().collect::<Vec<u8>>()).collect()
    };
    let patterns: Vec<Vec<u8>> = (0..4000).map(|_| word(&["%", "%", "_", "\\", "a", "b", "A", "é", "日", "%a", "a_", "ab"], 10)).collect();
    let texts: Vec<Vec<u8>> = (0..60).map(|_| word(&["a", "b", "A", "B", "é", "É", "日", "%", "_", "\\", "ab"], 16)).collect();
    for mode in [Mode::Like, Mode::ILike] {
        let (n, classes) = check(&texts, &patterns, mode);
        println!("{mode:?} random: {n} pairs, classes {classes:?}");
        assert!(classes[Class::General as usize] > 500, "the general matcher is exercised: {classes:?}");
    }
}

/// D4009: a pattern of 200000 `%` groups. The recursive port of MatchText took one stack frame per
/// group and ran a backend thread off its stack (SIGSEGV, the server); the iterative matcher
/// answers in place (PostgreSQL raises 54001 here; answering is the 'resource limits quiet' ruling).
#[test]
fn a_pattern_of_two_hundred_thousand_groups_answers_without_the_stack() {
    let mut buf = Buf::new(1 << 16);
    let mut r = buf.region();
    let text = vec![b'a'; 400_000];
    let groups = |g: &[u8], n: usize| -> Vec<u8> { g.repeat(n).into_iter().chain([b'%']).collect() };
    let (deep, under) = (groups(b"%a", 200_000), groups(b"%a_", 200_000));
    let ilike = Matcher::new(&deep, Mode::ILike, &mut r).unwrap();
    assert_eq!(ilike.class(), Class::General);
    assert_eq!(ilike.matches(&text), Ok(true));
    assert_eq!(ilike.matches(b"red"), Ok(false));
    let like = Matcher::new(&under, Mode::Like, &mut r).unwrap();
    assert_eq!(like.class(), Class::General);
    assert_eq!(like.matches(&text), Ok(true));
    assert_eq!(like.matches(&text[..399_999]), Ok(false), "one 'a' short");
}
