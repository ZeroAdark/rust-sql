//! D364 (M3 L0-h): `agg_sort` read a text key word's validity (`SK_TEXT` without `SK_SLOT`) from bit `w & 0xff` = 0 of the
//! NULL-flags word, which is key 0's flag. Under `(nullable k0, text t) ORDER BY k0 NULLS FIRST, t`, the groups whose k0
//! is NULL then compared t as two NULLs and fell through to the key words, ordered by code word instead of by t's text.
//! A text key's NULL is its representative's (`MINCODE_NONE` renders as none).
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::{Ctx, Site};
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_NULLKEY_SHIFT, SK_NULLS_FIRST, SK_TEXT, SK_TEXT_COLL_SHIFT};

/// The dictionary: code 0 `bing`, code 1 `map` (4-byte-header varlena images).
static ENTRIES: [[u8; 8]; 2] = [[(8 << 2) as u8, 0, 0, 0, b'b', b'i', b'n', b'g'], [(7 << 2) as u8, 0, 0, 0, b'm', b'a', b'p', 0]];

unsafe extern "C" fn dict(_s: *mut Site, _part: u32, _lane: u32, code: u32, out: *mut u64) -> i64 {
    *out = ENTRIES[code as usize].as_ptr() as u64;
    0
}

/// Keys: k0 (word 0, nullable), t's code word (word 1), the NULL-flags word (word 2, bit 0 = k0 NULL); slot 0: t's
/// representative code.
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::MinCode]);
const C_COLL: u64 = 950;

/// The four groups sorted by `k0 [NULLS FIRST | LAST], t`, as `(k0, t)`.
fn sorted(nulls_first: bool) -> Vec<(Option<u64>, &'static str)> {
    let words = LAYOUT.size() as usize / 8;
    // (k0, t's code, t's key word): the key words order `map` before `bing`, the text orders `bing` first
    let groups = [(None, 1u64, 5u64), (None, 0, 10), (Some(7u64), 1, 5), (Some(7), 0, 10)];
    let mut recs: Vec<Vec<u64>> = groups
        .iter()
        .map(|&(k0, code, word)| {
            let mut r = vec![0u64; words];
            (r[0], r[1], r[2]) = (k0.unwrap_or(0), word, k0.is_none() as u64);
            r[LAYOUT.slot_off[0] as usize / 8] = 3 << 32 | code;
            r
        })
        .collect();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: 4, kept: 4 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 3 << FORM_NULLKEY_SHIFT; // the flags word is key word 2
    desc[DESC_NSORT / 8] = 2;
    desc[DESC_SORT / 8] = if nulls_first { SK_NULLS_FIRST } else { 0 }; // k0: key word 0
    desc[DESC_SORT / 8 + 1] = SK_TEXT | C_COLL << SK_TEXT_COLL_SHIFT; // t: slot 0's code, lane 0 of site 0, under C
    // SAFETY: a site and a ctx are plain words; zero is their closed state.
    let mut site: Site = unsafe { core::mem::zeroed() };
    site.dict = Some(dict);
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    ctx.sites = &mut site;
    // SAFETY: `st` holds four live records of `LAYOUT`, `desc` a descriptor of it; site 0 renders through `dict`.
    let r = unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc.as_ptr() as *const u8) as u64) };
    assert_eq!(r, 0);
    ptrs.iter()
        .map(|&p| {
            // SAFETY: every pointer is one of `recs`.
            let r = unsafe { core::slice::from_raw_parts(p as *const u64, words) };
            let t = r[LAYOUT.slot_off[0] as usize / 8] as u32;
            ((r[2] & 1 == 0).then_some(r[0]), if t == 0 { "bing" } else { "map" })
        })
        .collect()
}

#[test]
fn a_text_key_after_a_null_int_key_orders_by_its_text() {
    let (null, seven) = (None, Some(7));
    assert_eq!(sorted(true), [(null, "bing"), (null, "map"), (seven, "bing"), (seven, "map")], "NULLS FIRST");
    assert_eq!(sorted(false), [(seven, "bing"), (seven, "map"), (null, "bing"), (null, "map")], "NULLS LAST");
}
