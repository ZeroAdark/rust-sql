//! D456 (M3-HOSTS): `agg_sort` read a key word's NULL at bit `w & 0xff` of the NULL-flags word, the key WORD's index, while
//! the image sets bit k for key k. A text key before a nullable key takes two words, so `GROUP BY t, k ORDER BY t, k`
//! read k's NULL at bit 2 (no key's) and ordered a NULL k as its zero word. The sort word now carries the key's bit
//! (`SK_NULL_SHIFT`, bit + 1; runs/topk_groups.rs `sort_words`), and every ASC / DESC x NULLS FIRST / LAST places it.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_NULLKEY_SHIFT, SK_DESC, SK_NULLS_FIRST, SK_NULL_SHIFT, SK_TEXT, SK_TEXT_COLL_SHIFT};

/// The dictionary: code 0 `bing`, code 1 `map`.
static ENTRIES: [[u8; 8]; 2] = [[(8 << 2) as u8, 0, 0, 0, b'b', b'i', b'n', b'g'], [(7 << 2) as u8, 0, 0, 0, b'm', b'a', b'p', 0]];

unsafe extern "C" fn dict(_s: *mut Site, _part: u32, _lane: u32, code: u32, out: *mut u64) -> i64 {
    *out = ENTRIES[code as usize].as_ptr() as u64;
    0
}

/// Keys: t's two words (0, 1), k (word 2, nullable, key 1), the NULL-flags word (word 3: bit 1 = k NULL); slot 0: t's code.
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::MinCode]);
const C_COLL: u64 = 950;

/// The six groups `(t, k)` sorted by `t, k [DESC] [NULLS FIRST | LAST]` with k's sort word `kw`.
fn sorted(desc_k: bool, nulls_first: bool, kw_bit: u64) -> Vec<(&'static str, Option<i64>)> {
    let words = LAYOUT.size() as usize / 8;
    // a NULL k's word is 0, between -4 and 9: read as a value it sorts in the middle
    let groups = [(1u64, Some(9i64)), (0, None), (1, None), (0, Some(-4)), (1, Some(-4)), (0, Some(9))];
    let mut recs: Vec<Vec<u64>> = groups
        .iter()
        .map(|&(code, k)| {
            let mut r = vec![0u64; words];
            (r[0], r[1], r[2], r[3]) = (code * 11, code, k.unwrap_or(0) as u64, (k.is_none() as u64) << 1);
            r[LAYOUT.slot_off[0] as usize / 8] = 3 << 32 | code;
            r
        })
        .collect();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: 6, kept: 6 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 4 << FORM_NULLKEY_SHIFT; // the flags word is key word 3
    desc[DESC_NSORT / 8] = 2;
    desc[DESC_SORT / 8] = SK_TEXT | C_COLL << SK_TEXT_COLL_SHIFT;
    desc[DESC_SORT / 8 + 1] = 2 | kw_bit | if desc_k { SK_DESC } else { 0 } | if nulls_first { SK_NULLS_FIRST } else { 0 };
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.dict = Some(dict);
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    // SAFETY: `st` holds six live records of `LAYOUT`, `desc` a descriptor of it; site 0 renders through `dict`.
    assert_eq!(unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc.as_ptr() as *const u8) as u64) }, 0);
    ptrs.iter()
        .map(|&p| {
            // SAFETY: every pointer is one of `recs`.
            let r = unsafe { core::slice::from_raw_parts(p as *const u64, words) };
            (if r[1] == 0 { "bing" } else { "map" }, (r[3] >> 1 & 1 == 0).then_some(r[2] as i64))
        })
        .collect()
}

#[test]
fn a_nullable_key_after_a_text_key_reads_its_own_null_bit() {
    let bit = 2 << SK_NULL_SHIFT; // key 1: bit 1, + 1
    for (desc_k, nulls_first) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut ks = vec![Some(-4), Some(9)];
        if desc_k {
            ks.reverse();
        }
        if nulls_first { ks.insert(0, None) } else { ks.push(None) }
        let want: Vec<(&str, Option<i64>)> = ["bing", "map"].iter().flat_map(|&t| ks.iter().map(move |&k| (t, k))).collect();
        assert_eq!(sorted(desc_k, nulls_first, bit), want, "DESC {desc_k} NULLS FIRST {nulls_first}");
    }
    // without the key's bit the word's own index (2) is read: no NULL is seen, a NULL k sorts as its 0 word
    assert_eq!(sorted(false, false, 0)[..3], [("bing", Some(-4)), ("bing", None), ("bing", Some(9))]);
}
