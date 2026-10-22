mod common;

use std::collections::HashMap;

use common::{Buf, VecDict};
use gravity_abi::hash::hash_bytes64 as hash_bytes;
use gravity_dict::{Derived, View};

fn first_byte(v: View, _: &mut gravity_dict::Region) -> Result<View, gravity_dict::Halt> {
    Ok(View::of(&unsafe { v.bytes() }[..1.min(v.len as usize)]))
}

#[test]
fn colliding_derived_values_share_one_canonical_index() {
    let d = VecDict::of(&["apple", "avocado", "banana", "blueberry", "cherry", "apricot", "", "coconut"]);
    let mut buf = Buf::new(8192);
    let mut r = buf.region();
    let t = Derived::build(&d, &mut r, &mut first_byte).unwrap();
    assert_eq!(t.ndistinct(), 4, "a b c and the empty string");
    let canon: Vec<u32> = (0..d.ncodes_pub()).map(|c| unsafe { t.canonical(c) }).collect();
    assert_eq!(canon, [0, 0, 1, 1, 2, 0, 3, 2]);
    let mut by_bytes: HashMap<Vec<u8>, u32> = HashMap::new();
    for c in 0..d.ncodes_pub() {
        let v = unsafe { t.view(c).bytes() }.to_vec();
        let k = unsafe { t.canonical(c) };
        assert_eq!(*by_bytes.entry(v.clone()).or_insert(k), k, "one index per value");
        assert_eq!(unsafe { t.hash(c) }, hash_bytes(&v));
    }
    assert_eq!(by_bytes.len(), 4);
    let mut out = vec![0u32; 8];
    let codes: Vec<u32> = (0..8).collect();
    unsafe { t.gather_canonical(codes.as_ptr(), core::ptr::null(), 8, out.as_mut_ptr()) }.unwrap();
    assert_eq!(out, canon);
}

#[test]
fn the_same_derived_value_hashes_alike_across_parts() {
    let p1 = VecDict::of(&["alpha", "beta", "gamma"]);
    let p2 = VecDict::of(&["bravo", "delta", "golf", "alfa"]);
    let mut b1 = Buf::new(8192);
    let mut b2 = Buf::new(8192);
    let (mut r1, mut r2) = (b1.region(), b2.region());
    let t1 = Derived::build(&p1, &mut r1, &mut first_byte).unwrap();
    let t2 = Derived::build(&p2, &mut r2, &mut first_byte).unwrap();
    let group = |t: &Derived, c: u32| (unsafe { t.hash(c) }, unsafe { t.view(c).bytes() }.to_vec());
    assert_eq!(group(&t1, 0), group(&t2, 3), "alpha / alfa → a");
    assert_eq!(group(&t1, 1), group(&t2, 0));
    assert_eq!(group(&t1, 2), group(&t2, 2));
    assert_ne!(group(&t1, 2), group(&t2, 1));
    assert_ne!(unsafe { t1.canonical(0) }, unsafe { t2.canonical(3) }, "canonical indices are per part; identity across parts is (hash, bytes)");
    let mut groups: HashMap<(u64, Vec<u8>), u32> = HashMap::new();
    for t in [&t1, &t2] {
        for c in 0..t.ncodes() {
            *groups.entry(group(t, c)).or_default() += 1;
        }
    }
    assert_eq!(groups.len(), 4);
    assert_eq!(groups[&(hash_bytes(b"a"), b"a".to_vec())], 2);
}

#[test]
fn a_large_dedup_stays_in_the_region_and_pops_its_scratch() {
    let words: Vec<String> = (0..20_000).map(|i| format!("k{}-{i}", i % 257)).collect();
    let refs: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    let d = VecDict::of(&refs);
    let mut buf = Buf::new(2 << 20);
    let mut r = buf.region();
    let t = Derived::build(&d, &mut r, &mut |v, _| {
        let b = unsafe { v.bytes() };
        Ok(View::of(&b[..b.iter().position(|&x| x == b'-').unwrap()]))
    })
    .unwrap();
    assert_eq!(t.ndistinct(), 257);
    let copied: usize = words.iter().map(|w| w.find('-').unwrap()).sum();
    assert_eq!(r.used(), 56 + 20_000 * (8 + 8 + 4) + copied, "header, offsets, hashes, canonical, the derived bytes (views outside the region are copied); the dedup map was popped");
    for c in 0..20_000u32 {
        assert_eq!(unsafe { t.canonical(c) }, unsafe { t.canonical(c % 257) });
    }
}

trait Ncodes {
    fn ncodes_pub(&self) -> u32;
}

impl Ncodes for VecDict {
    fn ncodes_pub(&self) -> u32 {
        self.entries.len() as u32
    }
}
