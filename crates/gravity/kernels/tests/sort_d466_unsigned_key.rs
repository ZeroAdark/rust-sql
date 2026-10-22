//! D466 (M3-GROUPED, V4R-135): `agg_sort` compared every key word signed, so a pg_lsn / xid8 group key (an unsigned
//! 8-byte image) at or past 2^63 sorted before 0. `SK_UNSIGNED` flips the sign bit before the compare, which is before any
//! DESC reversal; every ASC / DESC x NULLS FIRST / LAST places the values across 2^63 and the NULL as PostgreSQL does.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_NULLKEY_SHIFT, SK_DESC, SK_NULLS_FIRST, SK_UNSIGNED};

/// Key k (word 0, nullable, key 0), the NULL-flags word (word 1: bit 0 = k NULL); slot 0 a count.
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);

/// The groups sorted by k [DESC] [NULLS FIRST | LAST] with k's sort word flags `flags`.
fn sorted(desc: bool, nulls_first: bool, flags: u64) -> Vec<Option<u64>> {
    let words = LAYOUT.size() as usize / 8;
    let keys = [Some(1u64 << 63), Some(5), None, Some(u64::MAX), Some(0), Some((1u64 << 63) - 1)];
    let mut recs: Vec<Vec<u64>> = keys
        .iter()
        .map(|&k| {
            let mut r = vec![0u64; words];
            (r[0], r[1]) = (k.unwrap_or(0), k.is_none() as u64);
            r
        })
        .collect();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: keys.len() as u64, kept: keys.len() as u64 };
    let mut d = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    d[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    d[DESC_FORM / 8] = 2 << FORM_NULLKEY_SHIFT; // the flags word is key word 1
    d[DESC_NSORT / 8] = 1;
    d[DESC_SORT / 8] = flags | if desc { SK_DESC } else { 0 } | if nulls_first { SK_NULLS_FIRST } else { 0 };
    // SAFETY: a ctx is plain words; zero is its closed state (no text key reads a site).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds live records of `LAYOUT`, `d` a descriptor of it.
    assert_eq!(unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (d.as_ptr() as *const u8) as u64) }, 0);
    // SAFETY: every pointer is one of `recs`.
    ptrs.iter().map(|&p| unsafe { let r = core::slice::from_raw_parts(p as *const u64, 2); (r[1] == 0).then_some(r[0]) }).collect()
}

#[test]
fn an_unsigned_key_orders_across_two_to_the_63() {
    for (desc, nulls_first) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut want: Vec<Option<u64>> = [0, 5, (1u64 << 63) - 1, 1 << 63, u64::MAX].into_iter().map(Some).collect();
        if desc {
            want.reverse();
        }
        if nulls_first { want.insert(0, None) } else { want.push(None) }
        assert_eq!(sorted(desc, nulls_first, SK_UNSIGNED), want, "DESC {desc} NULLS FIRST {nulls_first}");
    }
    // without the flag the words compare signed: 2^63 and u64::MAX sort before 0
    assert_eq!(sorted(false, false, 0)[..2], [Some(1 << 63), Some(u64::MAX)]);
}
