//! B2-20: the code of a constant in a byte-ordered dictionary by binary search (`lookup::code_of`) and the code set's
//! table (`Bits::of_codes`), against the per-entry byte comparison they replace. P5-06 / SRC-17: a byte interval's code
//! run (`lookup::lower_bound`, `successor`, `prefix_run`) and its table (`Bits::of_run`), against the same comparison.
mod common;

use common::Buf;
use gravity_dict::lookup::{code_of, lower_bound, prefix_run, successor};
use gravity_dict::{Bits, Codes};

const SORTED: [&[u8]; 7] = [b"", b"a", b"ab", b"b", b"ba", b"z", b"\xc3\xa9"];

fn find(v: &[u8]) -> (Option<u32>, u32) {
    let mut reads = 0u32;
    let r = code_of::<()>(SORTED.len() as u32, v, &mut |c| {
        reads += 1;
        Ok(SORTED[c as usize])
    })
    .unwrap();
    (r, reads)
}

/// Every entry is found at its code and no other value is (between two entries, past either end, a prefix, a longer
/// value), each in at most ceil(log2(n + 1)) entry reads.
#[test]
fn every_entry_is_found_at_its_code_and_nothing_else_is() {
    let mut sorted = SORTED.to_vec();
    sorted.sort();
    assert_eq!(sorted, SORTED, "the fixture is in byte order");
    for (c, e) in SORTED.iter().enumerate() {
        let (r, reads) = find(e);
        assert_eq!(r, Some(c as u32), "{e:?}");
        assert!(reads <= 3, "{e:?}: {reads} reads");
    }
    for v in [&b"0"[..], b"aa", b"abc", b"bb", b"c", b"\xc3", b"\xc3\xa9\x00", b"\xff"] {
        let (r, reads) = find(v);
        assert_eq!(r, None, "{v:?}");
        assert!(reads <= 3, "{v:?}: {reads} reads");
    }
    assert_eq!(code_of::<()>(0, b"a", &mut |_| unreachable!()), Ok(None), "an empty dictionary");
    assert_eq!(code_of(3, b"a", &mut |c| if c == 1 { Err(7) } else { Ok(&b"x"[..]) }), Err(7), "a read error ends the search");
}

/// The code set's table holds exactly its codes; negated, every other code but the dictionary's NULL code; a code past
/// the dictionary is none of its; the tail past `ncodes` stays clear.
#[test]
fn a_code_set_table_holds_its_codes_or_their_complement() {
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let dict = Codes { ncodes: 70, null: Some(5) };
    let t = Bits::of_codes(&dict, &[1, 64, 69, 70, 900], false, &mut r).unwrap();
    assert_eq!((t.ncodes(), t.has_faults()), (70, false));
    assert_eq!((0..70).filter(|&c| unsafe { t.get(c) }).collect::<Vec<_>>(), vec![1, 64, 69]);
    let n = Bits::of_codes(&dict, &[1, 64], true, &mut r).unwrap();
    assert_eq!((0..70).filter(|&c| !unsafe { n.get(c) }).collect::<Vec<_>>(), vec![1, 5, 64], "the complement leaves the NULL code out");
    let tail = unsafe { *((*n.header()).data as *const u64).add(1) };
    assert_eq!(tail >> 6, 0, "no bit past code 69");
    // H08 (code.sparse's table form): a block base the table rejects for certain is a code outside the set; a code past
    // the dictionary is no answer
    assert_eq!((0..72).filter(|&c| t.rejects(c)).count(), 67);
    assert!(!t.rejects(1) && t.rejects(0) && t.rejects(5) && !t.rejects(70) && !t.rejects(900));
    assert!(n.rejects(1) && n.rejects(5) && !n.rejects(0), "negated: the set and the NULL code are rejected");
}

/// Against the sweep it replaces: over every subset of a small sorted dictionary's values (and values it does not hold),
/// the looked-up set's table answers each code as `values.contains(entry)` (negated: its opposite).
#[test]
fn the_looked_up_table_equals_the_byte_comparison_sweep() {
    let dict = Codes { ncodes: SORTED.len() as u32, null: None };
    let pool: [&[u8]; 9] = [b"", b"a", b"ab", b"ba", b"\xc3\xa9", b"zz", b"0", b"b", b"aa"];
    for mask in 0u32..1 << pool.len() {
        let values: Vec<&[u8]> = (0..pool.len()).filter(|i| mask >> i & 1 == 1).map(|i| pool[i]).collect();
        let codes: Vec<u32> = values.iter().filter_map(|v| code_of::<()>(SORTED.len() as u32, v, &mut |c| Ok(SORTED[c as usize])).unwrap()).collect();
        for negated in [false, true] {
            let mut buf = Buf::new(1024);
            let mut r = buf.region();
            let t = Bits::of_codes(&dict, &codes, negated, &mut r).unwrap();
            for (c, e) in SORTED.iter().enumerate() {
                assert_eq!(unsafe { t.get(c as u32) }, values.contains(e) != negated, "{values:?} negated {negated}: entry {e:?}");
            }
        }
    }
}

fn lb(dict: &[&[u8]], v: &[u8]) -> (u32, u32) {
    let mut reads = 0u32;
    let r = lower_bound::<()>(dict.len() as u32, v, &mut |c| {
        reads += 1;
        Ok(dict[c as usize])
    })
    .unwrap();
    (r, reads)
}

/// The lower bound is the first code at or after the value: in an empty dictionary, before the first entry, after the
/// last, equal to an entry, between two; against the sweep over every value of a pool, each in at most
/// ceil(log2(n + 1)) entry reads.
#[test]
fn lower_bound_is_the_first_code_at_or_after_the_value() {
    assert_eq!(lower_bound::<()>(0, b"a", &mut |_| unreachable!()), Ok(0), "an empty dictionary");
    let tail = &SORTED[1..];
    assert_eq!(lb(tail, b"").0, 0, "before the first entry");
    assert_eq!(lb(tail, b"0").0, 0, "before the first entry");
    assert_eq!(lb(&SORTED, b"\xff").0, SORTED.len() as u32, "after the last entry");
    assert_eq!(lb(&SORTED, b"\xc3\xa9\x00").0, SORTED.len() as u32, "after the last entry, the last one its prefix");
    for (c, e) in SORTED.iter().enumerate() {
        assert_eq!(lb(&SORTED, e).0, c as u32, "equal to entry {e:?}");
    }
    assert_eq!(lb(&SORTED, b"aa").0, 2, "between a and ab");
    assert_eq!(lb(&SORTED, b"abc").0, 3, "between ab and b");
    assert_eq!(lb(&SORTED, b"c").0, 5, "between ba and z");
    for v in [&b""[..], b"\x00", b"0", b"a", b"a\x00", b"aa", b"ab", b"abc", b"b", b"b\xff", b"ba", b"bb", b"y", b"z", b"z\x00", b"\xc3", b"\xc3\xa9", b"\xc4", b"\xff\xff"] {
        for dict in [&SORTED[..], tail, &SORTED[..1], &SORTED[3..5]] {
            let (r, reads) = lb(dict, v);
            assert_eq!(r as usize, dict.iter().position(|e| e >= &v).unwrap_or(dict.len()), "{v:?} in {dict:?}");
            assert!(reads <= 3, "{v:?}: {reads} reads");
        }
    }
    assert_eq!(lower_bound(3, b"a", &mut |c| if c == 1 { Err(7) } else { Ok(&b"x"[..]) }), Err(7), "a read error ends the search");
}

/// The byte successor bounds every string with the prefix: the last byte below 0xff raised, the 0xff bytes after it
/// dropped; a prefix of only 0xff bytes (or none) has none, and its run reaches the dictionary's end. Every prefix's run
/// is exactly the entries that start with it.
#[test]
fn a_prefix_is_the_run_up_to_its_byte_successor() {
    assert_eq!(successor(b"ab"), Some((&b"a"[..], b'c')));
    assert_eq!(successor(b"a\xff\xff"), Some((&b""[..], b'b')));
    assert_eq!(successor(b"\xc3\xa9"), Some((&b"\xc3"[..], 0xaa)));
    assert_eq!(successor(b"\xfe\xff"), Some((&b""[..], 0xff)));
    assert_eq!(successor(b"\xff\xff"), None);
    assert_eq!(successor(b""), None);
    let dict: [&[u8]; 9] = [b"a", b"a\xff", b"a\xff\xff", b"a\xff\xff\x00", b"b", b"\xfe\xff", b"\xff", b"\xff\xff", b"\xff\xff\x01"];
    let mut sorted = dict.to_vec();
    sorted.sort();
    assert_eq!(sorted, dict, "the fixture is in byte order");
    let run = |p: &[u8]| prefix_run::<()>(dict.len() as u32, p, &mut |c| Ok(dict[c as usize])).unwrap();
    assert_eq!(run(b"\xff"), (6, 9), "an all-0xff prefix runs to the end");
    assert_eq!(run(b"\xff\xff"), (7, 9));
    assert_eq!(run(b"a\xff"), (1, 4));
    assert_eq!(run(b"c"), (5, 5), "no entry has the prefix");
    for p in [&b""[..], b"a", b"a\xff", b"a\xff\xff", b"a\xff\xff\x00", b"a\x00", b"b", b"b\x00", b"\xfe", b"\xfe\xff", b"\xff", b"\xff\xff", b"\xff\xff\x01", b"\xff\xff\xff", b"0"] {
        let (lo, hi) = run(p);
        let want: Vec<u32> = (0..dict.len() as u32).filter(|&c| dict[c as usize].starts_with(p)).collect();
        assert_eq!((lo..hi).collect::<Vec<_>>(), want, "prefix {p:?}");
    }
    assert_eq!(prefix_run::<()>(0, b"a", &mut |_| unreachable!()), Ok((0, 0)), "an empty dictionary");
}

/// A run's table holds exactly its codes; negated, every other code but the dictionary's NULL code; cut at `ncodes`;
/// the tail past `ncodes` stays clear. Over every interval [from, to) of a pool, the run of the two lower bounds is the
/// byte comparison sweep's answer.
#[test]
fn a_run_table_holds_the_interval_or_its_complement() {
    for (n, null) in [(0u32, None), (1, None), (63, None), (64, None), (65, Some(64)), (130, Some(3)), (200, None)] {
        let dict = Codes { ncodes: n, null };
        for (lo, hi) in [(0, 0), (0, n), (1, n), (0, n + 5), (3, 2), (5, 70), (63, 64), (64, 128), (60, 131), (n, n + 1)] {
            for negated in [false, true] {
                let mut buf = Buf::new(4096);
                let mut r = buf.region();
                let t = Bits::of_run(&dict, lo, hi, negated, &mut r).unwrap();
                assert_eq!((t.ncodes(), t.has_faults()), (n, false));
                for c in 0..n {
                    let want = Some(c) != null && ((lo..hi).contains(&c) != negated);
                    assert_eq!(unsafe { t.get(c) }, want, "n {n} run [{lo}, {hi}) negated {negated}: code {c}");
                }
                if n % 64 != 0 {
                    let last = unsafe { *((*t.header()).data as *const u64).add(n as usize / 64) };
                    assert_eq!(last >> (n % 64), 0, "n {n} [{lo}, {hi}) negated {negated}: no bit past code {}", n - 1);
                }
            }
        }
    }
    let dict = Codes { ncodes: SORTED.len() as u32, null: None };
    let pool: [&[u8]; 8] = [b"", b"a", b"a\x00", b"ab", b"b", b"bz", b"z\x00", b"\xff"];
    for from in pool {
        for to in pool.iter().map(Some).chain([None]) {
            let lo = lb(&SORTED, from).0;
            let hi = to.map_or(SORTED.len() as u32, |t| lb(&SORTED, t).0);
            let mut buf = Buf::new(1024);
            let mut r = buf.region();
            let t = Bits::of_run(&dict, lo, hi, false, &mut r).unwrap();
            for (c, e) in SORTED.iter().enumerate() {
                assert_eq!(unsafe { t.get(c as u32) }, e >= &from && to.map_or(true, |t| e < t), "[{from:?}, {to:?}): entry {e:?}");
            }
        }
    }
}

/// P5-03: a table rejects every code exactly when no code's bit is set and no entry faulted: an empty run or code set
/// (the constant no entry of the part holds), never a run holding one code, a negated empty set or a faulted table.
#[test]
fn a_table_rejects_every_code_only_when_it_passes_none_and_faults_none() {
    for (n, null) in [(0u32, None), (1, None), (63, None), (64, Some(63)), (65, Some(64)), (130, Some(3))] {
        let dict = Codes { ncodes: n, null };
        for (lo, hi) in [(0, 0), (3, 2), (n, n + 1), (0, 1), (n.saturating_sub(1), n), (64, 65), (0, n)] {
            for negated in [false, true] {
                let mut buf = Buf::new(4096);
                let mut r = buf.region();
                let t = Bits::of_run(&dict, lo, hi, negated, &mut r).unwrap();
                let none = (0..n).all(|c| t.rejects(c));
                assert_eq!(t.rejects_all(), none, "n {n} run [{lo}, {hi}) negated {negated}");
            }
        }
        for codes in [&[][..], &[0u32][..], &[n.saturating_sub(1)][..], &[n + 3][..]] {
            let mut buf = Buf::new(4096);
            let mut r = buf.region();
            let t = Bits::of_codes(&dict, codes, false, &mut r).unwrap();
            assert_eq!(t.rejects_all(), (0..n).all(|c| t.rejects(c)), "n {n} codes {codes:?}");
        }
        let mut buf = Buf::new(4096);
        let mut r = buf.region();
        let t = Bits::all_faults(&dict, &mut r).unwrap();
        assert_eq!(t.rejects_all(), n == 0, "n {n}: a faulted entry is no rejection");
    }
}
