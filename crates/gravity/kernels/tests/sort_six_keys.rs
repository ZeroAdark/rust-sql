//! V4R-132 (M3-SORT): a full sort by more than four keys. `SortSpec` packs four keys' image tails; a key at 4 or past
//! compares by its `[flags, word]` pair alone (flags 0 = a NULLS FIRST null, 1 = a value, 2 = a NULLS LAST null; the
//! word the value's order image, DESC folded in), so keys 5 and 6 order by the pairs the producer wrote, DESC and NULLS
//! placement included.
use core::cmp::Ordering;
use gravity_kernels::host_abi::{cmp_rows, SortSpec};

/// (desc, nulls_first) per key: keys 5 and 6 (index 4, 5) DESC NULLS LAST and ASC NULLS FIRST, the rest mixed.
const DIRS: [(bool, bool); 6] = [(false, false), (true, true), (false, true), (true, false), (true, false), (false, true)];

/// The `[flags, word]` pair of `v` under `(desc, nulls_first)`, as runs/keylane.rs folds a word key.
fn pair(v: Option<i64>, (desc, nulls_first): (bool, bool)) -> [u64; 2] {
    match v {
        None => [if nulls_first { 0 } else { 2 }, 0],
        Some(x) => {
            let w = x as u64 ^ (1 << 63);
            [1, if desc { !w } else { w }]
        }
    }
}

/// PostgreSQL's order of one key: NULLS placement first, then the value (DESC reversing it).
fn pg(a: Option<i64>, b: Option<i64>, (desc, nulls_first): (bool, bool)) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => if nulls_first { Ordering::Less } else { Ordering::Greater },
        (Some(_), None) => if nulls_first { Ordering::Greater } else { Ordering::Less },
        (Some(x), Some(y)) => if desc { y.cmp(&x) } else { x.cmp(&y) },
    }
}

#[test]
fn keys_five_and_six_order_by_their_pairs_desc_and_nulls_included() {
    // every row equal on keys 1-4 except in small groups, so keys 5 and 6 decide most comparisons
    let vals = [None, Some(i64::MIN), Some(-7), Some(0), Some(3), Some(i64::MAX)];
    let mut rows: Vec<[Option<i64>; 6]> = Vec::new();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..600 {
        let mut r = [None; 6];
        for (k, c) in r.iter_mut().enumerate() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = if k < 4 { 2 } else { vals.len() };
            *c = vals[(seed >> 33) as usize % n];
        }
        rows.push(r);
    }
    // a name tail on key 0 would take spec bits 0..16; keys past four carry none (a nonzero spec for the first four)
    let spec = SortSpec::pack(&[(None, false), (None, true), (None, false), (None, true)]).unwrap();
    let words: Vec<[u64; 12]> = rows
        .iter()
        .map(|r| {
            let mut w = [0u64; 12];
            for k in 0..6 {
                w[2 * k..2 * k + 2].copy_from_slice(&pair(r[k], DIRS[k]));
            }
            w
        })
        .collect();
    let mut got: Vec<usize> = (0..rows.len()).collect();
    // SAFETY: each row is 12 words, the six keys' pairs.
    got.sort_by(|&a, &b| unsafe { cmp_rows(6, spec, words[a].as_ptr() as *const u8, words[b].as_ptr() as *const u8) });
    let mut want: Vec<usize> = (0..rows.len()).collect();
    want.sort_by(|&a, &b| (0..6).map(|k| pg(rows[a][k], rows[b][k], DIRS[k])).find(|o| o.is_ne()).unwrap_or(Ordering::Equal));
    let key = |i: &usize| rows[*i];
    assert_eq!(got.iter().map(key).collect::<Vec<_>>(), want.iter().map(key).collect::<Vec<_>>());
    assert_eq!(spec.key(4), (0, 0, false));
    assert_eq!(spec.key(5), (0, 0, false));
}
