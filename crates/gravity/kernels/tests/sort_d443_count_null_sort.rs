//! D443: `agg_sort` read a `Count` slot's NULL from its validity bit. A `count(*) FILTER (..)` no row of the group passed
//! (or a `count(x)` over only NULLs) leaves the bit unset, so under `ORDER BY count DESC` (NULLS FIRST) its 0 sorted first
//! as a NULL; PostgreSQL's count is 0, never NULL. The sort key's NULL must be exactly the output's (lowerv4
//! fold/class_word.rs `finish`, fold/slots.rs `slot_valid`): a count never, an average where its count word is 0, every
//! other slot by its validity. For each slot kind a top over group finals sorts (runs_topk_groups `sort_words`), two
//! groups, one no row reached, sorted DESC NULLS FIRST and ASC NULLS LAST, against the order that rule gives.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_LAYOUT, DESC_NSORT, DESC_SORT, SK_DESC, SK_NULLS_FIRST, SK_SLOT};

/// Two groups over `layout` (key word 0: 1 and 2): group 1 folded no row into slot 0 (zeroed, its validity bit unset; the sentinel
/// counts one row in both);
/// group 2 folded one row of value 1. Sorted by slot 0 (`desc`: DESC NULLS FIRST, else ASC NULLS LAST): the keys in order.
fn sorted(layout: RecordLayout, desc: bool) -> Vec<u64> {
    let words = layout.size() as usize / 8;
    let sentinel = layout.sentinel_off() as usize / 8;
    let mut recs: Vec<Vec<u64>> = (1..=2u64).map(|k| {
        let mut r = vec![0u64; words];
        r[0] = k;
        r[sentinel] = 1; // every opened record counts a row in its sentinel
        r
    }).collect();
    let off = layout.slot_off(0) as usize / 8;
    let r = &mut recs[1];
    match layout.slot_kind(0) {
        SlotKind::AvgI128 => (r[off], r[off + 2]) = (1, 1),
        _ => r[off] = 1,
    }
    if layout.has_validity() {
        let (o, bit) = layout.valid_bit(0);
        let bytes = r.as_mut_ptr() as *mut u8;
        // SAFETY: the validity byte lies inside the record's `words` words.
        unsafe { *bytes.add(o) |= 1 << bit };
    }
    let mut ptrs: Vec<*const u8> = recs.iter().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: 2, kept: 2 };
    let mut d = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = layout.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_NSORT / 8] = 1;
    d[DESC_SORT / 8] = SK_SLOT | if desc { SK_DESC | SK_NULLS_FIRST } else { 0 };
    // SAFETY: a ctx is plain words; zero is its closed state (no text key renders).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds two live records of `layout`, `d` a descriptor of it.
    assert_eq!(unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (d.as_ptr() as *const u8) as u64) }, 0);
    // SAFETY: every pointer is one of `recs`.
    ptrs.iter().map(|&p| unsafe { *(p as *const u64) }).collect()
}

/// The final of group 1 (no row reached slot 0): `None` where PostgreSQL's final is NULL. A count is 0; a sum the layout
/// gives no validity bit is one no row can leave NULL (its argument NOT NULL, every opened record folded a row): 0.
fn empty_final(kind: SlotKind, validity: bool) -> Option<i64> {
    (kind == SlotKind::Count || (kind == SlotKind::SumI64 && !validity)).then_some(0)
}

/// The order PostgreSQL gives the two groups: NULL first under DESC NULLS FIRST, last under ASC NULLS LAST; a count of 0
/// below a count of 1.
fn expected(kind: SlotKind, validity: bool, desc: bool) -> Vec<u64> {
    let empty_first = match empty_final(kind, validity) {
        None => desc,
        Some(v) => (v < 1) != desc,
    };
    if empty_first { vec![1, 2] } else { vec![2, 1] }
}

#[test]
fn a_slot_sorts_null_exactly_where_its_final_is_null() {
    let keys = [KeyKind::I64];
    let mut bad = Vec::new();
    for kind in [SlotKind::Count, SlotKind::SumI64, SlotKind::MinI64, SlotKind::MaxI64, SlotKind::AvgI128] {
        let mut layouts = vec![RecordLayout::with_sentinel(&keys, &[kind, SlotKind::Count], true, Some(1))];
        if !matches!(kind, SlotKind::MinI64 | SlotKind::MaxI64) {
            layouts.push(RecordLayout::with_sentinel(&keys, &[kind, SlotKind::Count], false, Some(1)));
        }
        for layout in layouts {
            for desc in [true, false] {
                let (got, want) = (sorted(layout, desc), expected(kind, layout.has_validity(), desc));
                if got != want {
                    bad.push(format!("{kind:?} validity={} desc={desc}: {got:?}, PostgreSQL {want:?}", layout.has_validity()));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
