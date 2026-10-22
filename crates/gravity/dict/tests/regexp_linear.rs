//! V4R-163: `Run::run_linear` (a program whose every class run is possessive) against the recursive backtracker it
//! replaces, differentially: the match, its span, the groups, and the memo's `read` / `saw_end` equal for every subject,
//! over random subjects built from each pattern's own bytes, and no-match cases. Patterns: a URL host's plus others,
//! unanchored and several-group ones among them; the near misses (a class run before a literal starting inside the class,
//! a class before an overlapping class, before an optional literal, before `.+`, a `.` before a literal) must not be
//! linear, and still answer as before.
use gravity_dict::regexp::{compile, replace_first, replace_first_memo, Memo, Outcome, Program, Template};
use gravity_dict::Region;

const LINEAR: &[&[u8]] = &[
    br"^https?://(?:m\.)?([^/]+)/.*$",
    br"^([a-z]+)@.*$",
    br"^(?:m\.)?([^.]+)\..*$",
    br"^([0-9]+)-.*",
    br"^[A-Z]{2}-([0-9]{3,5})x$",
    br"^(?:ab)?(?:cd)?([^;]*);.*$",
    br"^([a-z]+)[0-9].*$",
    br"://([^/]+)/",
    br"^/([^/]+)/([^/]+)/.*$",
    br"([a-z]+)@([a-z]+)\.",
];

/// One matching subject per `LINEAR` pattern, mutated by `subject` into near matches.
const EXAMPLES: &[&[u8]] = &[b"https://m.a.b/c", b"ab@x", b"m.foo.bar", b"123-z", b"XY-1234x", b"abcdfoo;bar", b"abc5x", b"http://a.b/c", b"/ab/cd/e", b"x ab@cd.ef"];

const NOT_LINEAR: &[&[u8]] = &[
    br"^([a-z]+)a.*$",
    br"^([a-z0-9]+)[0-9].*$",
    br"^([^/]+)(?:/)?x.*$",
    br"^([a-z]*).+$",
    br"^[A-Z0-9]{2,3}([0-9]{3,5})x$",
    br"^(.+)/([^/]*)$",
    br"([a-z.]+)\.com",
];

/// The outcome as a comparable value: the span, every group, `read`, `saw_end`.
type Key = Option<((usize, usize), Vec<Option<(usize, usize)>>, usize, bool)>;

fn key(p: &Program, o: Outcome) -> Key {
    match o {
        Outcome::Match(f) => Some(((f.start, f.end), (1..=p.ngroups()).map(|g| f.group(g)).collect(), f.read, f.saw_end)),
        Outcome::NoMatch => None,
        Outcome::Refused => panic!("a refusal on a short subject"),
    }
}

fn both(p: &Program, s: &[u8]) -> (Key, Key) {
    (key(p, p.find(s, 0)), key(p, p.find_recursive(s, 0)))
}

/// xorshift64*
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A subject from the pattern's own literal bytes, a few class-edge bytes and multi-byte UTF-8, sometimes a prefix of a
/// matching shape so the deep paths run.
fn subject(r: &mut Rng, pat: &[u8]) -> Vec<u8> {
    let mut alpha: Vec<u8> = pat.iter().copied().filter(|b| !b"^$()[]?+*{}\\,".contains(b)).collect();
    alpha.extend_from_slice(b"aZ09/.@;-xhw:");
    let mut s = Vec::new();
    if let Some(k) = LINEAR.iter().position(|p| *p == pat).filter(|_| r.below(2) == 0) {
        // a near match: the example with up to two bytes replaced, inserted or deleted
        s.extend_from_slice(EXAMPLES[k]);
        for _ in 0..r.below(3) {
            let i = r.below(s.len() + 1);
            let b = alpha[r.below(alpha.len())];
            match r.below(3) {
                0 if i < s.len() => s[i] = b,
                1 => s.insert(i, b),
                _ if i < s.len() => {
                    s.remove(i);
                }
                _ => {}
            }
        }
        return s;
    }
    if r.below(3) == 0 {
        s.extend_from_slice([&b"http://"[..], b"https://m.", b"ab", b"abcd", b"m.", b"XY-123"][r.below(6)]);
    }
    for _ in 0..r.below(24) {
        match r.below(12) {
            0 => s.extend_from_slice("é".as_bytes()),
            1 => s.extend_from_slice("日".as_bytes()),
            _ => s.push(alpha[r.below(alpha.len())]),
        }
    }
    s
}

#[test]
fn the_linear_runner_answers_as_the_backtracker_does() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for pat in LINEAR {
        let p = compile(pat).unwrap_or_else(|| panic!("{} compiles", String::from_utf8_lossy(pat)));
        assert!(p.linear(), "{} is linear", String::from_utf8_lossy(pat));
        let (mut hit, mut miss) = (0, 0);
        for _ in 0..100_000 {
            let s = subject(&mut r, pat);
            let (a, b) = both(&p, &s);
            assert_eq!(a, b, "{} over {:?}", String::from_utf8_lossy(pat), String::from_utf8_lossy(&s));
            match a {
                None => miss += 1,
                Some(_) => hit += 1,
            }
        }
        assert!(hit > 1000 && miss > 1000, "{}: {hit} matches, {miss} misses", String::from_utf8_lossy(pat));
    }
}

#[test]
fn a_near_miss_is_not_linear_and_answers_as_before() {
    let mut r = Rng(7);
    for pat in NOT_LINEAR {
        let p = compile(pat).unwrap_or_else(|| panic!("{} compiles", String::from_utf8_lossy(pat)));
        assert!(!p.linear(), "{} must not be linear", String::from_utf8_lossy(pat));
        for _ in 0..20_000 {
            let s = subject(&mut r, pat);
            let (a, b) = both(&p, &s);
            assert_eq!(a, b);
        }
    }
}

/// The memo over a byte-sorted corpus (a dictionary's order) answers `replace_first` entry for entry under the linear
/// runner, duplicates and the empty subject included.
#[test]
fn the_memo_over_the_linear_runner_answers_replace_first() {
    let mut r = Rng(11);
    let (p, t) = (compile(LINEAR[0]).unwrap(), Template::parse(br"\1").unwrap());
    // hosts shared by runs of entries (a dictionary's byte order puts one host's URLs together), plus random subjects
    let hosts: Vec<Vec<u8>> = (0..300).map(|_| subject(&mut r, b"abcxyz.")).collect();
    let mut ents: Vec<Vec<u8>> = (0..50_000)
        .map(|i| {
            if i % 4 == 0 {
                return subject(&mut r, LINEAR[0]);
            }
            let mut e = [&b"http://"[..], b"https://", b"https://m."][r.below(3)].to_vec();
            e.extend_from_slice(&hosts[r.below(hosts.len())]);
            e.extend_from_slice(&subject(&mut r, b"/q=1&"));
            e
        })
        .collect();
    ents.push(Vec::new());
    ents.sort();
    let mut buf = vec![0u64; 1 << 12];
    let mut memo = Memo::new();
    for e in &ents {
        let mut reg = unsafe { Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let a = unsafe { replace_first_memo(&p, &t, br"\1", e, &mut reg, &mut memo).unwrap().unwrap().bytes().to_vec() };
        let mut reg = unsafe { Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let b = unsafe { replace_first(&p, &t, br"\1", e, &mut reg).unwrap().unwrap().bytes().to_vec() };
        assert_eq!(a, b, "{:?}", String::from_utf8_lossy(e));
    }
    assert!(memo.hits > 1000);
}
