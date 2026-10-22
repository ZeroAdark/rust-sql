use super::*;
use ::mcx::MemoryContext;
use ::regex_spencer::REG_ADVANCED;

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        postgres_seams::check_for_interrupts::set(|| Ok(()));
    });
    mbutils::SetDatabaseEncoding(wchar::PG_UTF8).unwrap();
}

fn auto(p: &str) -> Option<Re2Pattern> {
    setup();
    set_regex_engine(REGEX_ENGINE_AUTO);
    dispatch(p.as_bytes(), REG_ADVANCED, b"clean subject").unwrap()
}

fn replace(p: &str, s: &str, r: &str, start: i32, n: i32) -> String {
    let re = auto(p).expect("pattern should dispatch to re2");
    let cx = MemoryContext::new("test");
    let out =
        replace_text_regexp_re2(cx.mcx(), &re, s.as_bytes(), r.as_bytes(), start, n).unwrap();
    String::from_utf8(out.as_slice().to_vec()).unwrap()
}

const URL_HOST_PAT: &str = r"^https?://(?:m\.)?([^/]+)/.*$";

#[test]
fn url_host_capture_replaces_with_the_host() {
    setup();
    if !re2_available() {
        return;
    }
    assert_eq!(replace(URL_HOST_PAT, "http://m.example.com/path/x?y=1", r"\1", 0, 1), "example.com");
    assert_eq!(replace(URL_HOST_PAT, "https://sub.host.ru/", r"\1", 0, 1), "sub.host.ru");
    assert_eq!(replace(URL_HOST_PAT, "not-a-url", r"\1", 0, 1), "not-a-url");
    assert_eq!(replace(URL_HOST_PAT, "http://hostonly.com", r"\1", 0, 1), "http://hostonly.com");
    assert_eq!(replace(URL_HOST_PAT, "", r"\1", 0, 1), "");
}

#[test]
fn replacement_escapes() {
    setup();
    if !re2_available() {
        return;
    }
    assert_eq!(replace(r"([a-z]+) ([a-z]+)", "abc def", r"\2 \1", 0, 1), "def abc");
    assert_eq!(replace("a", "xay", r"[\&]", 0, 1), "x[a]y");
    assert_eq!(replace("a", "xay", r"\\", 0, 1), "x\\y");
    // Unknown escape keeps the backslash (PG behavior).
    assert_eq!(replace("a", "xay", r"\z", 0, 1), "x\\zy");
    // Trailing lone backslash.
    assert_eq!(replace("a", "xay", "b\\", 0, 1), "xb\\y");
    // Group that did not participate appends nothing.
    assert_eq!(replace("foo(bar)?", "foo", r"[\1]", 0, 1), "[]");
    // Group index beyond the pattern's groups appends nothing.
    assert_eq!(replace("(f)oo", "foo", r"\9x", 0, 1), "x");
}

#[test]
fn glob_nth_start() {
    setup();
    if !re2_available() {
        return;
    }
    // n == 0: replace all.
    assert_eq!(replace("[0-9]", "a1b2c3", "#", 0, 0), "a#b#c#");
    // n-th match only.
    assert_eq!(replace("[0-9]", "a1b2c3", "#", 0, 2), "a1b#c3");
    // start offset (characters).
    assert_eq!(replace("[0-9]", "a1b2c3", "#", 2, 1), "a1b#c3");
    // Empty matches advance one character.
    assert_eq!(replace("x*", "abc", "-", 0, 0), "-a-b-c-");
    // Multibyte: empty-match advance is per character, not per byte.
    assert_eq!(replace("x*", "é", "-", 0, 0), "-é-");
}

#[test]
fn longest_match_semantics() {
    setup();
    if !re2_available() {
        return;
    }
    // Spencer's all-greedy rule is leftmost-LONGEST; RE2 must run in POSIX
    // longest mode, not Perl leftmost-first, for the classes we admit.
    assert_eq!(replace("a|ab", "abc", "#", 0, 1), "#c");
    assert_eq!(replace("(a+|a)(b?)", "aab", r"[\1|\2]", 0, 1), "[aa|b]");
}

#[test]
fn auto_fails_closed() {
    setup();
    set_regex_engine(REGEX_ENGINE_AUTO);
    // Incompatible constructs classify to Spencer (None), never error.
    for p in [r"(a)\1", r"\w+", "a*?", "[[:alpha:]]"] {
        assert!(dispatch(p.as_bytes(), REG_ADVANCED, b"x").unwrap().is_none(), "{p}");
    }
    // Classifier-admitted but RE2-rejected patterns also fail closed.
    // (POSIX leading-] brackets are rejected upstream by the classifier; use
    // forced mode to confirm compile errors surface only when forced.)
    set_regex_engine(REGEX_ENGINE_SPENCER);
    assert!(dispatch(b"anything(?=x)", REG_ADVANCED, b"x").unwrap().is_none());
}

#[test]
fn auto_fails_closed_on_data() {
    setup();
    if !re2_available() {
        return;
    }
    set_regex_engine(REGEX_ENGINE_AUTO);
    let subjects: &[&[u8]] = &[
        b"\x00",
        b"\x00abc",
        b"abc\x00",
        b"ab\x00cd",
        b"caf\xc3\xa9\x00dcba",
        b"caf\xc3\x00dcba",
        b"caf\xc3",
        b"a\xffb",
        b"\xc3\x28",
        b"\xed\xa0\x80xyz",
        b"\xc0\xafabc",
    ];
    for s in subjects {
        assert!(!subject_compatible(s), "{s:?}");
        assert!(dispatch(b"a", REG_ADVANCED, s).unwrap().is_none(), "{s:?}");
    }
    for s in [&b""[..], b"abc", "café".as_bytes(), b"a\nb\tc"] {
        assert!(subject_compatible(s), "{s:?}");
        assert!(dispatch(b"a", REG_ADVANCED, s).unwrap().is_some(), "{s:?}");
    }
    // The data guard applies after the cached pattern verdict, per subject.
    assert!(dispatch(b"a", REG_ADVANCED, b"a\x00b").unwrap().is_none());
    assert!(dispatch(b"a", REG_ADVANCED, b"ab").unwrap().is_some());
}

#[test]
fn forced_re2_bypasses_data_guard() {
    setup();
    if !re2_available() {
        return;
    }
    // The testing knob exposes raw RE2 byte semantics, NUL data included —
    // this is what lets tests observe the divergence auto guards against.
    set_regex_engine(REGEX_ENGINE_RE2);
    let re = dispatch(b"b.d", REG_ADVANCED, b"ab\x00d!").unwrap();
    set_regex_engine(REGEX_ENGINE_AUTO);
    assert!(re.expect("forced re2 dispatches").is_match(b"ab\x00d!", 0));
}

#[test]
fn forced_re2_errors_name_engine() {
    setup();
    if !re2_available() {
        return;
    }
    set_regex_engine(REGEX_ENGINE_RE2);
    let err = dispatch(br"(a)\1", REG_ADVANCED, b"x").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("regex_engine=re2"), "{msg}");
    set_regex_engine(REGEX_ENGINE_AUTO);
}

#[test]
fn quoted_mode_is_literal() {
    setup();
    if !re2_available() {
        return;
    }
    set_regex_engine(REGEX_ENGINE_AUTO);
    let re =
        dispatch(br"a.c", ::regex_spencer::REG_QUOTE, b"x").unwrap().expect("quoted dispatches");
    assert!(re.is_match(b"xa.cy", 0));
    assert!(!re.is_match(b"xabcy", 0));
}

#[test]
fn dispatch_decision_is_cached() {
    setup();
    if !re2_available() {
        return;
    }
    set_regex_engine(REGEX_ENGINE_AUTO);
    let a = dispatch(URL_HOST_PAT.as_bytes(), REG_ADVANCED, b"x").unwrap().unwrap();
    let b = dispatch(URL_HOST_PAT.as_bytes(), REG_ADVANCED, b"x").unwrap().unwrap();
    // Same Rc-backed compiled pattern comes back from the cache.
    assert!(Rc::ptr_eq(&a.inner, &b.inner));
    // Spencer verdicts are cached too.
    assert!(dispatch(br"\d", REG_ADVANCED, b"x").unwrap().is_none());
    assert!(dispatch(br"\d", REG_ADVANCED, b"x").unwrap().is_none());
}

// The pattern program against the RE2 automaton it stands in for: for every
// pattern the program compiles, at every start, the verdict and every group
// span equal RE2's (the program refusing is the RE2 answer).
#[cfg(have_re2)]
#[test]
fn pattern_program_vs_re2_differential() {
    setup();
    set_regex_engine(REGEX_ENGINE_AUTO);

    let patterns: &[&str] = &[
        URL_HOST_PAT,
        "^",
        "^$",
        "^abc",
        "^abc$",
        "^a?b",
        "^https?://",
        r"^(?:m\.)?([^/]+)$",
        "^([a-z]+)z$",
        "^([a-z]+)bc$",
        "^([^a]+)y$",
        "^([^a]+)[^b]+X$",
        "^[0-9]{4}-[0-9]{2}$",
        "^x*y+z?",
        "^a.*",
        "^a.+$",
        "^([0-9]{2,4})x",
        r"^\.\*",
        "^é?x",
        "^[^,]*,",
        "https?://(?:www\\.)?([^/]+)/.*$",
        "://([^/]+)/",
        "([a-z]+)/([a-z]+)",
        "^(.*)/(.*)$",
        "^(.+)/[0-9]+/.*$",
        "a.b",
        ".{2,3}x",
        "^([^/]{2,3})",
        "([A-Za-z]+)/v?[0-9.]+",
        "^([A-Za-z]+)/v?([0-9]+)\\..*$",
        "[0-9]+",
        "b*",
        "x*$",
        "é(.)",
        "(.)(.)(.)",
        "://(x)/(y)",
        "^(.)x(.)$",
    ];
    let subjects: &[&str] = &[
        "",
        "http://m.example.com/path/x?y=1",
        "https://sub.host.ru/",
        "http://hostonly.com",
        "https://пример.рф/страница",
        "http://m/",
        "not-a-url",
        "abc",
        "abcz",
        "abcdef",
        "xéy",
        "xéX",
        "éxé",
        "m.x",
        "m",
        "1234-56",
        "0123x",
        "xxyyzz",
        "a",
        "ab",
        ".*x",
        ",,a,,b,,",
        "line one\nline two\n",
        "café instrument",
        "Mozilla/5.0 (X11; Linux x86_64) Chrome/118.0.1",
        "a/b/c",
        "x/y/12/z",
        "日本語/abc/12/",
        "a\nb",
        "bbb",
    ];
    for p in patterns {
        let re = dispatch(p.as_bytes(), REG_ADVANCED, b"clean").unwrap().unwrap_or_else(|| panic!("pattern {p:?} should dispatch to re2"));
        if !re.has_program() {
            continue;
        }
        for s in subjects {
            for start in (0..=s.len()).filter(|&i| s.is_char_boundary(i)) {
                for nout in [0usize, 1, 2, 4, 10] {
                    let mut prog_out = [(-7i64, -7i64); 10];
                    let mut re2_out = [(-7i64, -7i64); 10];
                    let prog_m = re.exec(s.as_bytes(), start, &mut prog_out[..nout]);
                    let re2_m = re.inner.match_at(s.as_bytes(), start, &mut re2_out[..nout]);
                    assert_eq!(prog_m, re2_m, "match verdict diverges: {p:?} on {s:?} from {start}");
                    if prog_m {
                        assert_eq!(prog_out[..nout], re2_out[..nout], "group spans diverge: {p:?} on {s:?} from {start}");
                    }
                }
            }
        }
    }
}

// Pathological backtracking: the step budget refuses (exec falls back to RE2,
// the same answer), never a wrong answer or a hang.
#[cfg(have_re2)]
#[test]
fn pattern_program_budget_fallback() {
    setup();
    set_regex_engine(REGEX_ENGINE_AUTO);
    let p = "^[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*[ab]*c$";
    let re = dispatch(p.as_bytes(), REG_ADVANCED, b"clean").unwrap().unwrap();
    let hay = "ab".repeat(64);
    assert_eq!(program::Program::compile(p.as_bytes()).unwrap().exec(hay.as_bytes(), 0, &mut []), None);
    assert!(!re.exec(hay.as_bytes(), 0, &mut []));
    assert!(!re.inner.match_at(hay.as_bytes(), 0, &mut []));
}

// The program attaches under auto dispatch for the subset RE2 runs
// leftmost-first (pinned at both ends, or without a quantifier: anchored or
// not, several groups); whole-match-tier, alternation and longest-mode
// patterns get none.
#[test]
fn pattern_program_attaches() {
    setup();
    if !re2_available() {
        return;
    }
    set_regex_engine(REGEX_ENGINE_AUTO);
    for p in [URL_HOST_PAT, "^([a-z]+)/([a-z]+)$", "^(.*)/(.*)$", "://(x)/(y)"] {
        let re = dispatch(p.as_bytes(), REG_ADVANCED, b"clean").unwrap().unwrap();
        assert!(re.has_program(), "{p:?} must compile to a pattern program");
    }
    for p in ["^(foo|bar)$", "(a)+", "a|ab", "://([^/]+)/"] {
        if let Some(re) = dispatch(p.as_bytes(), REG_ADVANCED, b"clean").unwrap() {
            assert!(!re.has_program(), "{p:?} must not get a program");
        }
    }
}

#[test]
fn guc_backing_is_session_scoped() {
    set_regex_engine(REGEX_ENGINE_SPENCER);
    std::thread::spawn(|| assert_eq!(regex_engine(), REGEX_ENGINE_AUTO)).join().unwrap();
    assert_eq!(regex_engine(), REGEX_ENGINE_SPENCER);
    set_regex_engine(REGEX_ENGINE_AUTO);
}
