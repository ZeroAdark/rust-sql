mod common;

use common::{Buf, VecDict};
use gravity_abi::Lane;
use gravity_abi::hash::hash_bytes64 as hash_bytes;
use gravity_dict::{publish, Bits, BuildError, Claim, Derived, Hash, Key, OnceTable, View, ELEM_BITS, ELEM_I64, I64};

const WORDS: &[&str] = &["", "a", "ab", "abc", "abcd", "b", "bc", "hello", "HELLO", "world", "héllo", "日本語"];

#[test]
fn bits_table_answers_the_predicate_per_code_and_per_window() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let t = Bits::build(&d, &mut r, &mut |v| Ok(unsafe { v.bytes() }.len() > 3)).unwrap();
    assert_eq!(t.ncodes(), WORDS.len() as u32);
    assert!(!t.has_faults());
    for (i, w) in WORDS.iter().enumerate() {
        assert_eq!(unsafe { t.get(i as u32) }, w.len() > 3, "{w}");
    }
    let codes: Vec<u32> = (0..130).map(|i| (i * 7 % WORDS.len()) as u32).collect();
    let mut valid = vec![!0u64; 3];
    valid[0] &= !0b110;
    let mut out = vec![0u64; 3];
    unsafe { t.gather_mask(codes.as_ptr(), valid.as_ptr(), 130, true, &mut out) }.unwrap();
    for i in 0..130 {
        let expect = WORDS[codes[i] as usize].len() > 3 && !(1..=2).contains(&i);
        assert_eq!((out[i / 64] >> (i % 64)) & 1 == 1, expect, "row {i}");
    }
    assert_eq!(out[2] >> 2, 0, "rows past 130 are clear");
    let mut and_in = vec![0u64; 3];
    and_in[0] = 1 << 3 | 1 << 4;
    unsafe { t.gather_mask(codes.as_ptr(), core::ptr::null(), 130, false, &mut and_in) }.unwrap();
    assert_eq!(and_in[0], (((WORDS[codes[3] as usize].len() > 3) as u64) << 3) | (((WORDS[codes[4] as usize].len() > 3) as u64) << 4));
    assert_eq!((and_in[1], and_in[2]), (0, 0));
    let narrow: Vec<u16> = codes.iter().map(|&c| c as u16).collect();
    let wide: Vec<u64> = codes.iter().map(|&c| c as u64).collect();
    let (mut o16, mut o64) = (vec![0u64; 3], vec![0u64; 3]);
    unsafe {
        t.gather_mask(narrow.as_ptr(), valid.as_ptr(), 130, true, &mut o16).unwrap();
        t.gather_mask(wide.as_ptr(), valid.as_ptr(), 130, true, &mut o64).unwrap();
    }
    assert_eq!((o16, o64), (out.clone(), out));
}

#[test]
fn i64_table_gathers_values_and_keeps_the_lanes_validity() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let t = I64::build(&d, &mut r, &mut |v| Ok(v.len as i64)).unwrap();
    assert_eq!(unsafe { t.get(d.code_of("日本語")) }, 9);
    let codes = [3u32, 0, 11, 7];
    let valid = [0b1011u64];
    let mut out = [-1i64; 4];
    unsafe { t.gather(codes.as_ptr(), valid.as_ptr(), 4, out.as_mut_ptr()) }.unwrap();
    assert_eq!(out, [3, 0, -1, 5]);
}

#[test]
fn hash_table_is_the_crate_hash_of_the_derived_view() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let t = Hash::build(&d, &mut r, &mut |v| Ok(v)).unwrap();
    for (i, w) in WORDS.iter().enumerate() {
        assert_eq!(unsafe { t.get(i as u32) }, hash_bytes(w.as_bytes()));
    }
    let lower = Hash::build(&d, &mut r, &mut |v| Ok(View::of(if unsafe { v.bytes() } == b"HELLO" { b"hello" } else { unsafe { v.bytes() } }))).unwrap();
    assert_eq!(unsafe { lower.get(d.code_of("HELLO")) }, unsafe { lower.get(d.code_of("hello")) });
    assert_ne!(hash_bytes(b"a"), hash_bytes(b"b"));
    assert_ne!(hash_bytes(b""), hash_bytes(b"\0"));
    let mut out = [0u64; 2];
    unsafe { t.gather([1u32, 5].as_ptr(), core::ptr::null(), 2, out.as_mut_ptr()) }.unwrap();
    assert_eq!(out, [hash_bytes(b"a"), hash_bytes(b"b")]);
}

#[test]
fn derived_table_stores_views_in_the_region_or_copies_them() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(8192);
    let mut r = buf.region();
    let t = Derived::build(&d, &mut r, &mut |v, region| {
        let b = unsafe { v.bytes() };
        if b.len() % 2 == 0 {
            let out = region.alloc(b.len(), 1)?;
            for (i, &x) in b.iter().enumerate() {
                unsafe { *out.add(i) = x.to_ascii_uppercase() };
            }
            Ok(View { ptr: out, len: b.len() as u32 })
        } else {
            Ok(View::of(&b[..1]))
        }
    })
    .unwrap();
    assert_eq!(unsafe { t.view(d.code_of("abcd")).bytes() }, b"ABCD");
    assert_eq!(unsafe { t.view(d.code_of("abc")).bytes() }, b"a");
    assert_eq!(unsafe { t.view(d.code_of("")).bytes() }, b"");
    assert_eq!(unsafe { t.hash(d.code_of("abcd")) }, hash_bytes(b"ABCD"));
    assert!(t.ndistinct() < t.ncodes());
}

#[test]
fn the_null_code_is_never_evaluated() {
    let mut d = VecDict::of(WORDS);
    d.null = Some(2);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let mut seen = Vec::new();
    let t = Bits::build(&d, &mut r, &mut |v| {
        seen.push(unsafe { v.bytes() }.to_vec());
        Ok(true)
    })
    .unwrap();
    assert!(!seen.contains(&b"ab".to_vec()));
    assert!(!unsafe { t.get(2) });
    assert_eq!(seen.len(), WORDS.len() - 1);
}

#[test]
fn region_exhaustion_is_reported_and_a_larger_region_succeeds() {
    let d = VecDict::of(WORDS);
    let mut small = Buf::new(64);
    let mut r = small.region();
    match I64::build(&d, &mut r, &mut |v| Ok(v.len as i64)) {
        Err(BuildError::RegionFull { needed }) => assert!(needed > 0),
        other => panic!("{other:?}"),
    }
    let mut big = Buf::new(4096);
    let mut r = big.region();
    let before = r.used();
    let t = I64::build(&d, &mut r, &mut |v| Ok(v.len as i64)).unwrap();
    assert_eq!(r.used() - before, 56 + WORDS.len() * 8, "header + one word per code, nothing else");
    assert_eq!(unsafe { t.get(3) }, 3);
    let mark = r.mark();
    r.alloc(100, 8).unwrap();
    r.reset_to(mark);
    assert_eq!(r.used(), mark);
}

#[test]
fn a_table_round_trips_through_a_lane() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(4096);
    let mut r = buf.region();
    let bits = Bits::build(&d, &mut r, &mut |v| Ok(v.len == 1)).unwrap();
    let vals = I64::build(&d, &mut r, &mut |v| Ok(v.len as i64)).unwrap();
    let mut l = Lane::NONE;
    unsafe { publish(bits.header(), 0, 3, &mut l) };
    assert!(gravity_dict::lane::is_table(&l));
    assert_eq!((gravity_dict::lane::elem(&l), gravity_dict::lane::over(&l), l.ncodes_or_scale), (ELEM_BITS, 3, WORDS.len() as u32));
    assert_eq!(gravity_abi::ctx::lane::id_kind(l.id), gravity_abi::ctx::lane::ID_TABLE);
    let back = unsafe { Bits::from_lane(&l) }.expect("a bits lane");
    assert!(unsafe { back.get(1) } && !unsafe { back.get(0) });
    assert!(unsafe { I64::from_lane(&l) }.is_none(), "the element type is checked");
    unsafe { publish(vals.header(), 1, 3, &mut l) };
    assert_eq!(gravity_dict::lane::elem(&l), ELEM_I64);
    assert_eq!(unsafe { I64::from_lane(&l) }.map(|t| unsafe { t.get(7) }), Some(5));
    assert!(unsafe { Bits::from_lane(&Lane::NONE) }.is_none());
    assert_eq!(gravity_dict::lane::deferred(1, 2, gravity_dict::Deferred { row: 9, code: 0x345 }), gravity_abi::status::fault(1, 2, 0x345));
}

#[test]
fn once_table_builds_once_per_key_and_generation() {
    let d = VecDict::of(WORDS);
    let mut buf = Buf::new(1 << 16);
    let mut r = buf.region();
    let once = OnceTable::in_region(&mut r, 2).unwrap();
    once.begin_query(1);
    let k = Key { part: 3, epoch: 9, fingerprint: 0xfeed };
    let Claim::Build(b) = once.claim(k) else { panic!("first claimer builds") };
    assert_eq!(b.key(), k);
    let t = Bits::build(&d, &mut r, &mut |v| Ok(v.len == 0)).unwrap();
    b.publish(t.header());
    let Claim::Ready(p) = once.claim(k) else { panic!("second claimer reads") };
    assert_eq!(p, t.header());
    assert_eq!(once.live(), 1);
    let k2 = Key { part: 3, epoch: 10, fingerprint: 0xfeed };
    let Claim::Build(b2) = once.claim(k2) else { panic!("a new epoch is a new table") };
    drop(b2);
    let Claim::Build(b2) = once.claim(k2) else { panic!("a failed build is retried by the next claimer") };
    b2.publish(t.header());
    assert!(matches!(once.claim(Key { part: 4, epoch: 9, fingerprint: 1 }), Claim::Full));
    once.begin_query(2);
    assert_eq!(once.live(), 0);
    let Claim::Build(b3) = once.claim(k) else { panic!("a new query never sees the old table") };
    b3.publish(t.header());
    assert!(matches!(once.claim(k), Claim::Ready(_)));
}

#[test]
fn concurrent_claimers_wait_for_the_one_builder() {
    let mut buf = Buf::new(1 << 16);
    let mut r = buf.region();
    let once = OnceTable::in_region(&mut r, 4).unwrap();
    once.begin_query(7);
    let d = VecDict::of(WORDS);
    let table = Bits::build(&d, &mut r, &mut |_| Ok(true)).unwrap();
    let hdr = table.header() as usize;
    let builds = std::sync::atomic::AtomicUsize::new(0);
    let k = Key { part: 1, epoch: 1, fingerprint: 42 };
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| match once.claim(k) {
                Claim::Build(b) => {
                    builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    b.publish(hdr as *const _);
                }
                Claim::Ready(p) => assert_eq!(p as usize, hdr),
                Claim::Full => panic!("full"),
                Claim::Held | Claim::Cancelled => panic!("no claimer holds or cancels"),
            });
        }
    });
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(once.live(), 1);
}

/// A dictionary whose entries lie in one payload, as a resident part's do (the fused build's `over`).
struct PayloadEntries {
    payload: Vec<u8>,
    spans: Vec<(usize, usize)>,
    null: Option<u32>,
}

impl gravity_dict::Dict for PayloadEntries {
    fn ncodes(&self) -> u32 {
        self.spans.len() as u32
    }

    fn null_code(&self) -> Option<u32> {
        self.null
    }

    fn entries(&self, f: &mut dyn FnMut(u32, View)) {
        for (i, &(s, e)) in self.spans.iter().enumerate() {
            f(i as u32, View::of(&self.payload[s..e]));
        }
    }
}

/// The fused derived fingerprint: `Fp128::build_derived_over` answers what the two-step build answers (`Derived::build_over`, then
/// `Fp128::build_by_canon` over its canonical indexes) code for code, over a URL host capture's program with its memo: runs of one
/// host, hosts that recur apart (http / https / m.), subjects the pattern does not match (their own bytes), an empty
/// capture, the null code and a faulting entry ([0, 0], and a fault recorded, in both). A view outside the payload is
/// refused, never hashed. The fused table keeps nothing but its 16 bytes a code.
#[test]
fn the_fused_derived_fingerprint_equals_the_two_step_build() {
    use gravity_dict::regexp::{compile, replace_first_memo, Memo, Template};
    use gravity_dict::table::Halt;
    use gravity_dict::{Codes, Fault, Fp128, Region};
    gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let mut ents = common::url_corpus(4000, 11);
    for s in ["https://example.com/a", "https://m.example.com/b", "http://nohost", "https://", "http:///x", "ftp://x/y", "", "https://shop.example.org"] {
        ents.push(s.as_bytes().to_vec());
    }
    ents.sort();
    ents.dedup();
    let (mut payload, mut spans) = (Vec::new(), Vec::new());
    for e in &ents {
        spans.push((payload.len(), payload.len() + e.len()));
        payload.extend_from_slice(e);
    }
    let null = Some(ents.iter().position(|e| e.as_slice() == b"ftp://x/y").unwrap() as u32);
    let d = PayloadEntries { payload, spans, null };
    let bad = ents.iter().position(|e| e.as_slice() == b"http://nohost").unwrap();
    let (prog, tmpl) = (compile(br"^https?://(?:m\.)?([^/]+)/.*$").unwrap(), Template::parse(br"\1").unwrap());
    let derive = |memo: &mut Memo, v: View, region: &mut Region| -> Result<View, Halt> {
        let b = unsafe { v.bytes() };
        if b == ents[bad].as_slice() {
            return Err(Halt::Fault(Fault(1)));
        }
        Ok(replace_first_memo(&prog, &tmpl, br"\1", b, region, memo)?.expect("no refusal"))
    };
    let n = ents.len();
    let (mut b1, mut b2) = (Buf::new(64 * n + (1 << 16)), Buf::new(64 * n + (1 << 16)));
    let (mut r1, mut r2) = (b1.region(), b2.region());
    let mut m1 = Memo::new();
    let two = Derived::build_over(&d, &mut r1, &d.payload, &mut |v, r| derive(&mut m1, v, r)).unwrap();
    let codes = Codes { ncodes: two.ncodes(), null: d.null };
    let want = Fp128::build_by_canon(&codes, &mut r1, two.ndistinct(), &mut |c| unsafe { two.canonical(c) }, &mut |c| unsafe { two.view(c) }).unwrap();
    let mut m2 = Memo::new();
    let got = Fp128::build_derived_over(&d, &mut r2, &d.payload, &mut |v, r| derive(&mut m2, v, r)).unwrap();
    assert!(two.ndistinct() < two.ncodes() - 2, "hosts repeat");
    for c in 0..n as u32 {
        assert_eq!(unsafe { got.get(c) }, unsafe { want.get(c) }, "code {c}: {:?}", String::from_utf8_lossy(&ents[c as usize]));
    }
    assert_eq!(unsafe { got.get(bad as u32) }, [0, 0]);
    assert_eq!(unsafe { got.get(d.null.unwrap()) }, [0, 0]);
    assert!(got.has_faults() && want.has_faults());
    assert!(r2.used() <= 16 * n + 1024 + n.div_ceil(64) * 8, "16 bytes a code: {} for {n}", r2.used());
    let mut b3 = Buf::new(64 * n + (1 << 16));
    let mut r3 = b3.region();
    let outside = b"elsewhere";
    let e = Fp128::build_derived_over(&d, &mut r3, &d.payload, &mut |_, _| Ok(View::of(outside)));
    assert!(matches!(e, Err(BuildError::OutsideBase)), "{e:?}");
}

/// L-DERIVE (V4R-30): `Fp128::build_derived_seq` pulled with a URL host capture's program and memo, a memo hit on the previous
/// subject's prefix reusing its fingerprint unread (`Memo::sets`), answers `build_derived_over` code for code: runs of one
/// host, hosts that recur apart, unmatched subjects, an empty capture, the null code ([0, 0]); a code past the table is
/// skipped. A memo hit after a miss that left the memo unset (the value differs from the previous subject's) must hash:
/// were `same` taken from the hit alone, "https://a.example/x" after "ftp://b/y" would read the ftp entry's print.
#[test]
fn the_pulled_derived_fingerprint_equals_the_swept_one() {
    use gravity_dict::regexp::{compile, replace_first_memo, Memo, Template};
    use gravity_dict::{Fp128, Region};
    gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let mut ents = common::url_corpus(4000, 11);
    for s in ["https://example.com/a", "https://m.example.com/b", "http://nohost", "https://", "http:///x", "ftp://x/y", "", "https://shop.example.org"] {
        ents.push(s.as_bytes().to_vec());
    }
    ents.sort();
    ents.dedup();
    // out of byte order (a dictionary that is not byte-ranked): a hit on a prefix a miss in between did not set
    for s in ["https://a.example/x", "ftp://b/y", "https://a.example/z"] {
        ents.push(s.as_bytes().to_vec());
    }
    let (mut payload, mut spans) = (Vec::new(), Vec::new());
    for e in &ents {
        spans.push((payload.len(), payload.len() + e.len()));
        payload.extend_from_slice(e);
    }
    let null = Some(ents.iter().position(|e| e.as_slice() == b"ftp://x/y").unwrap() as u32);
    let d = PayloadEntries { payload, spans, null };
    let (prog, tmpl) = (compile(br"^https?://(?:m\.)?([^/]+)/.*$").unwrap(), Template::parse(br"\1").unwrap());
    let n = ents.len();
    let (mut b1, mut b2) = (Buf::new(64 * n + (1 << 16)), Buf::new(64 * n + (1 << 16)));
    let (mut r1, mut r2) = (b1.region(), b2.region());
    let mut m1 = Memo::new();
    let want = Fp128::build_derived_over(&d, &mut r1, &d.payload, &mut |v, r| Ok(replace_first_memo(&prog, &tmpl, br"\1", unsafe { v.bytes() }, r, &mut m1)?.expect("no refusal"))).unwrap();
    let (mut m2, mut code, mut last_set) = (Memo::new(), 0usize, false);
    let mut scratch = vec![0u64; 1024];
    let mut sr = unsafe { Region::new(scratch.as_mut_ptr() as *mut u8, scratch.len() * 8) };
    let got = Fp128::build_derived_seq(&d, &mut r2, &d.payload, || {
        let &(s, e) = d.spans.get(code)?;
        code += 1;
        let (hits, sets) = (m2.hits, m2.sets);
        let v = match replace_first_memo(&prog, &tmpl, br"\1", &d.payload[s..e], &mut sr, &mut m2) {
            Ok(v) => v.expect("no refusal"),
            Err(e) => return Some(Err(e)),
        };
        let same = m2.hits != hits && last_set;
        last_set = m2.hits != hits || m2.sets != sets;
        Some(Ok(((code - 1) as u32, unsafe { v.bytes() }, same)))
    })
    .unwrap();
    assert!(m2.hits > 0 && m2.sets > 0, "the memo answered some subjects");
    for c in 0..n as u32 {
        assert_eq!(unsafe { got.get(c) }, unsafe { want.get(c) }, "code {c}: {:?}", String::from_utf8_lossy(&ents[c as usize]));
    }
    assert_eq!(unsafe { got.get(d.null.unwrap()) }, [0, 0]);
}

/// V4R-160: `Fp128::build_derived_seq` writes its table unzeroed, so a region full of old bytes (a reused build buffer)
/// must still read [0, 0] for every code the sequence does not print: the null code, codes it skips (a gap), a code past
/// the table, codes after the sequence ends early, and a code pulled again out of order keeps its last print.
#[test]
fn the_pulled_derived_table_zeroes_every_unprinted_code_over_a_dirty_region() {
    use gravity_dict::{Fp128, Region};
    gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let ents: Vec<Vec<u8>> = (0..200u32).map(|i| format!("host{}.example/{}", i % 17, i).into_bytes()).collect();
    let (mut payload, mut spans) = (Vec::new(), Vec::new());
    for e in &ents {
        spans.push((payload.len(), payload.len() + e.len()));
        payload.extend_from_slice(e);
    }
    let d = PayloadEntries { payload, spans, null: Some(3) };
    let (k0, k1) = gravity_dict::fp::seed().unwrap();
    // pulled: 0..=9 (3 is the null code), a gap 10..=19, 20..=149, a code past the table, 5 again, then the end (150.. never pulled)
    let mut order: Vec<u32> = (0..10).chain(20..150).collect();
    order.insert(order.len() - 3, 10_000);
    order.push(5);
    for fill in [0u64, u64::MAX, 0xA5A5_A5A5_A5A5_A5A5] {
        let mut buf = common::Buf::new(64 * 200 + (1 << 16));
        buf.0.iter_mut().for_each(|w| *w = fill);
        let mut r = buf.region();
        let mut i = 0usize;
        let t = Fp128::build_derived_seq(&d, &mut r, &d.payload, || {
            let &c = order.get(i)?;
            i += 1;
            let (s, e) = d.spans.get(c as usize).copied().unwrap_or((0, 0));
            Some(Ok((c, &d.payload[s..e], false)))
        })
        .unwrap();
        for c in 0..200u32 {
            let printed = c != 3 && (c < 10 || (20..150).contains(&c));
            let (s, e) = d.spans[c as usize];
            let want = if printed { gravity_dict::fp::siphash128(k0, k1, &d.payload[s..e]) } else { [0, 0] };
            assert_eq!(unsafe { t.get(c) }, want, "fill {fill:#x} code {c}");
        }
    }
}
