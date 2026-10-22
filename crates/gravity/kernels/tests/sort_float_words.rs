//! V4R-111 (M3-WORDS): `agg_sort` compared a float group key's word (its canonical float8 bits) and a float slot as a
//! signed integer, so negative floats came out in reverse (-1 before -2). An `SK_FLOAT` word orders by the float's total
//! order: -0 = +0, every NaN equal and above +Infinity, NULLs by the word's NULLS flag, DESC reversing the values only.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_NULLKEY_SHIFT, SK_DESC, SK_FLOAT, SK_NULLS_FIRST, SK_SLOT};

/// Keys: the float key (word 0, nullable), the NULL-flags word (word 1, bit 0 = the key is NULL); slot 0 a `sum(float8)`,
/// slot 1 an `avg(float)` accumulator {N, Sx, Sxx}.
const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::F64, KeyKind::I64], &[SlotKind::SumF64, SlotKind::F64Accum]);

/// The values in PostgreSQL's ascending float8 order (-0 and 0 tie: `float8_cmp_internal`).
const VALUES: [f64; 9] = [f64::NEG_INFINITY, -2.5, -1.0, -0.0, 1e-300, 1.0, 2.5, f64::INFINITY, f64::NAN];

/// The records' values (None = NULL) sorted by the one sort word `w` (slot / key word per `w`), as the values back.
fn sorted(values: &[Option<f64>], w: u64) -> Vec<Option<f64>> {
    let words = LAYOUT.size() as usize / 8;
    let mut recs: Vec<Vec<u64>> = values
        .iter()
        .map(|&x| {
            let mut r = vec![0u64; words];
            let v = x.unwrap_or(0.0);
            (r[0], r[1]) = (v.to_bits(), x.is_none() as u64);
            r[LAYOUT.slot_off[0] as usize / 8] = v.to_bits();
            // avg = Sx / N with N = 4: the slot orders by Sx / 4 = v
            let a = LAYOUT.slot_off[1] as usize / 8;
            (r[a], r[a + 1]) = (if x.is_some() { 4.0f64 } else { 0.0 }.to_bits(), (4.0 * v).to_bits());
            r
        })
        .collect();
    let n = recs.len();
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: n as u64, kept: n as u64 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 2 << FORM_NULLKEY_SHIFT; // the flags word is key word 1
    desc[DESC_NSORT / 8] = 1;
    desc[DESC_SORT / 8] = w;
    // SAFETY: a ctx is plain words; zero is its closed state (no text word reads a site).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    // SAFETY: `st` holds `n` live records of `LAYOUT`, `desc` a descriptor of it.
    assert_eq!(unsafe { agg_sort(&mut ctx as *mut _ as u64, (&mut st as *mut EmitState as *mut u8) as u64, (desc.as_ptr() as *const u8) as u64) }, 0);
    ptrs.iter()
        .map(|&p| {
            // SAFETY: every pointer is one of `recs`.
            let r = unsafe { core::slice::from_raw_parts(p as *const u64, words) };
            (r[1] == 0).then(|| f64::from_bits(r[0]))
        })
        .collect()
}

/// `a` and `b` as the same float8 values (NaN = NaN, and -0 = 0 as a tie under `ties`).
fn same(a: &[Option<f64>], b: &[Option<f64>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| match (x, y) {
        (Some(x), Some(y)) => x.is_nan() && y.is_nan() || x == y,
        (None, None) => true,
        _ => false,
    })
}

#[test]
fn float_words_order_totally_with_nulls_placed() {
    // shuffled input: the NULL, then the values out of order (a +0 beside the -0)
    let input: Vec<Option<f64>> = [None, Some(1.0), Some(f64::NAN), Some(-1.0), Some(-2.5), Some(0.0), Some(f64::INFINITY), Some(-0.0), Some(1e-300), Some(2.5), Some(f64::NEG_INFINITY)].to_vec();
    let asc: Vec<Option<f64>> = {
        let mut v: Vec<Option<f64>> = VALUES.iter().map(|&x| Some(x)).collect();
        v.insert(4, Some(0.0)); // +0 ties -0: either order is PG's
        v
    };
    for base in [0u64, SK_SLOT, SK_SLOT | 1] {
        for (desc, nulls_first) in [(false, false), (false, true), (true, false), (true, true)] {
            let w = base | SK_FLOAT | if desc { SK_DESC } else { 0 } | if nulls_first { SK_NULLS_FIRST } else { 0 };
            // a sum(float8) slot of a layout without validity words is never NULL: the NULL row is left out there
            let nullable = base != SK_SLOT;
            let mut want = asc.clone();
            if desc {
                want.reverse();
            }
            if nullable {
                if nulls_first { want.insert(0, None) } else { want.push(None) }
            }
            let got = sorted(&input[if nullable { 0 } else { 1 }..], w);
            // -0 / +0 tie: compare as a multiset over that tie, exactly elsewhere
            let norm = |v: &[Option<f64>]| v.iter().map(|x| x.map(|f| f + 0.0)).collect::<Vec<_>>();
            assert!(same(&norm(&got), &norm(&want)), "word {w:#x}: {got:?}");
        }
    }
}

#[test]
fn without_the_flag_a_negative_float_word_orders_as_an_integer() {
    // the M1 order the flag replaces: -1.0's bits compare below -2.5's as signed integers
    let got = sorted(&[Some(-1.0), Some(-2.5)], 0);
    assert!(same(&got, &[Some(-1.0), Some(-2.5)]), "{got:?}");
    let fixed = sorted(&[Some(-1.0), Some(-2.5)], SK_FLOAT);
    assert!(same(&fixed, &[Some(-2.5), Some(-1.0)]), "{fixed:?}");
}
