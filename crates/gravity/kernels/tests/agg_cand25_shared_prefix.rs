//! CAND-25 past a shared prefix: `agg_sort`'s text-first road takes each first text image's head from the eight bytes
//! past the prefix every image shares, so keys that share a long one (zero-padded numbers) are told apart by their heads
//! and not each by the comparator. The order must be the bytes' order whatever the prefix: an image that IS the prefix,
//! images that differ only past the head, NULLs, DESC, a second text word, a top's cut.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, SK_DESC, SK_HEAP, SK_NULLS_FIRST, SK_TEXT, SK_TEXT_SLOT_SHIFT};

/// Key word 0: the group's own word (the last tie-break); slots 0 and 1: two text keys' kept copies (0: NULL).
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinVar, SlotKind::MinVar]);

/// A 4-byte-header varlena image of `s`.
fn varlena(s: &[u8]) -> Vec<u8> {
    let mut v = (((s.len() + 4) as u32) << 2).to_le_bytes().to_vec();
    v.extend_from_slice(s);
    v
}

/// The keys: (first text key, second text key), `None` a NULL.
type Key = (Option<Vec<u8>>, Option<Vec<u8>>);

/// `keys` (record i's key word `10_000 - i`, so the key words alone would order them backwards) sorted by `agg_sort` over
/// both text words (`desc` / `nulls_first` on the first), the first `bound` when some; as indices into `keys`.
fn sorted(keys: &[Key], desc: bool, nulls_first: bool, bound: Option<usize>) -> Vec<usize> {
    let words = LAYOUT.size() as usize / 8;
    let images: Vec<(Option<Vec<u8>>, Option<Vec<u8>>)> = keys.iter().map(|(a, b)| (a.as_deref().map(varlena), b.as_deref().map(varlena))).collect();
    let mut recs: Vec<Vec<u64>> = images
        .iter()
        .enumerate()
        .map(|(i, (a, b))| {
            let mut r = vec![0u64; words];
            r[0] = 10_000 - i as u64;
            r[LAYOUT.slot_off[0] as usize / 8] = a.as_ref().map_or(0, |v| v.as_ptr() as u64);
            r[LAYOUT.slot_off[1] as usize / 8] = b.as_ref().map_or(0, |v| v.as_ptr() as u64);
            r
        })
        .collect();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: keys.len() as u64, kept: keys.len() as u64 };
    let mut d = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_FORM / 8] = bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT);
    d[DESC_NSORT / 8] = 2;
    d[DESC_SORT / 8] = SK_TEXT | SK_HEAP | if desc { SK_DESC } else { 0 } | if nulls_first { SK_NULLS_FIRST } else { 0 };
    d[DESC_SORT / 8 + 1] = SK_TEXT | SK_HEAP | 1 << SK_TEXT_SLOT_SHIFT;
    // SAFETY: a ctx is plain words; zero is its closed state (heap words render nothing).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds live records of `LAYOUT` whose slots point at live varlenas; `d` is a descriptor of it.
    let r = unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (d.as_ptr() as *const u8) as u64) };
    assert_eq!(r, 0);
    let n = bound.unwrap_or(keys.len()).min(keys.len());
    ptrs[..n].iter().map(|&p| 10_000 - unsafe { *(p as *const u64) } as usize).collect()
}

/// The order `agg_sort` must give: the first key by its bytes (NULLs last, or first under `nulls_first`; DESC reverses
/// two values), the second by its bytes (NULLs last), then the key word ascending.
fn expected(keys: &[Key], desc: bool, nulls_first: bool) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    let null_rank = |v: &Option<Vec<u8>>, first: bool| match v {
        Some(_) => 1,
        None if first => 0,
        None => 2,
    };
    idx.sort_by(|&x, &y| {
        let (a, b) = (&keys[x], &keys[y]);
        null_rank(&a.0, nulls_first)
            .cmp(&null_rank(&b.0, nulls_first))
            .then_with(|| match (&a.0, &b.0) {
                (Some(p), Some(q)) => if desc { q.cmp(p) } else { p.cmp(q) },
                _ => core::cmp::Ordering::Equal,
            })
            .then_with(|| null_rank(&a.1, false).cmp(&null_rank(&b.1, false)))
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| (10_000 - x).cmp(&(10_000 - y)))
    });
    idx
}

/// Zero-padded numbers behind one letter (61 bytes, the first 55 shared by every key), some twice with different
/// second keys, one key that is the shared prefix itself, two that differ only past the eight bytes after the prefix,
/// and NULLs.
fn keys() -> Vec<Key> {
    let mut v: Vec<Key> = Vec::new();
    let mut x: u64 = 0x9e37_79b9;
    for i in 0..3000u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let n = (x >> 33) % 150_000;
        let k = format!("k{:0>60}", n).into_bytes();
        let w = if i % 11 == 0 { None } else { Some(format!("w{}", (x >> 20) % 7).into_bytes()) };
        v.push((Some(k), w));
        if i % 97 == 0 {
            v.push((None, Some(b"w0".to_vec())));
        }
    }
    let prefix = format!("k{}", "0".repeat(54)).into_bytes();
    v.push((Some(prefix.clone()), Some(b"w1".to_vec())));
    let mut far1 = prefix.clone();
    far1.extend_from_slice(b"12345678a");
    let mut far2 = prefix.clone();
    far2.extend_from_slice(b"12345678b");
    v.push((Some(far2), Some(b"w2".to_vec())));
    v.push((Some(far1), Some(b"w3".to_vec())));
    v
}

#[test]
fn keys_sharing_a_long_prefix_sort_as_their_bytes() {
    let k = keys();
    for (desc, nf) in [(false, false), (true, false), (false, true), (true, true)] {
        let want = expected(&k, desc, nf);
        assert_eq!(sorted(&k, desc, nf, None), want, "whole sort desc {desc} nulls_first {nf}");
        assert_eq!(sorted(&k, desc, nf, Some(10)), want[..10], "a top's cut of 10, desc {desc} nulls_first {nf}");
        assert_eq!(sorted(&k, desc, nf, Some(1)), want[..1], "a top's cut of 1, desc {desc} nulls_first {nf}");
    }
}

#[test]
fn keys_that_are_all_one_value_or_prefixes_of_one_another() {
    let same: Vec<Key> = (0..50).map(|i| (Some(b"abcdefghijklmnop".to_vec()), Some(format!("w{}", i % 3).into_bytes()))).collect();
    assert_eq!(sorted(&same, false, false, None), expected(&same, false, false));
    let nested: Vec<Key> = (0..40).rev().map(|n| (Some(b"x".repeat(n + 1)), None)).collect();
    assert_eq!(sorted(&nested, false, false, None), expected(&nested, false, false), "each image a prefix of the next");
    assert_eq!(sorted(&nested, true, false, Some(5)), expected(&nested, true, false)[..5]);
    let few: Vec<Key> = (0..600u64).map(|i| (Some(format!("{}{}{}{}", "x".repeat(20), i % 3, "y".repeat(10), (i * 7919) % 401).into_bytes()), None)).collect();
    assert_eq!(sorted(&few, false, false, None), expected(&few, false, false), "three heads past the prefix: the comparator's road");
    assert_eq!(sorted(&few, true, false, Some(7)), expected(&few, true, false)[..7]);
    let short: Vec<Key> = vec![(Some(b"pre".to_vec()), None), (Some(b"prefix-b".to_vec()), None), (Some(b"pre\0".to_vec()), None), (Some(b"prefix-a".to_vec()), None), (Some(Vec::new()), None)];
    assert_eq!(sorted(&short, false, false, None), expected(&short, false, false), "an empty image shares no byte");
}
