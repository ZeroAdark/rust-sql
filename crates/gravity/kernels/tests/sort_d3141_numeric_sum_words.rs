//! D3141: `agg_sort` orders records by a numeric sum (sum(numeric) as `SumNum` / `SumNumWide`, sum(int8) as `SumI128`)
//! through word rows when every record's sum fits one word (`num_words`), and through the generic comparator otherwise.
//! Both roads must give the generic comparator's order: every record for a whole sort, the first `n` under a bound, the
//! tie lead under WITH TIES. Sums: small mantissas with ties, -Infinity / +Infinity / NaN, the OVERFLOW sentinel (first in
//! either direction), NULL; a mantissa past 2^62 sends the sort to the comparator. Each ASC / DESC, NULLS FIRST / LAST,
//! mixed with an int8 key word and a count slot.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS, NUM_SENT_HI};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, agg_sort_generic, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, FORM_NULLKEY_SHIFT, FORM_TIES, SK_DESC, SK_NULLS_FIRST, SK_SLOT};

/// Keys: an int8 key (word 0), the NULL-flags word (word 1: its high bits the record's index, so the key rows are unique).
/// Slots: 0 sum(numeric) (`SumNum`), 1 a wide sum(numeric) (`SumNumWide`), 2 sum(int8) (`SumI128`), 3 count.
const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumNum, SlotKind::SumNumWide, SlotKind::SumI128, SlotKind::Count]);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A sum: `Ok(mantissa)` or `Err(code)` (1 NaN, 2 +Infinity, 3 -Infinity, 4 OVERFLOW).
type Sum = Result<i128, u64>;

fn pick(rng: &mut Rng, big: bool) -> Sum {
    let small = [-7i128, -1, 0, 0, 3, 3, 1 << 40, -(1 << 61), (1 << 62) - 1, -(1 << 62)];
    match rng.below(14) {
        0 => Err(1),
        1 => Err(2),
        2 => Err(3),
        3 => Err(4),
        4 if big => Ok(if rng.below(2) == 0 { 1 << 62 } else { -(1i128 << 100) }),
        _ => Ok(small[rng.below(small.len() as u64) as usize]),
    }
}

/// `m` records; `big`: some sums past one word.
fn records(rng: &mut Rng, m: usize, big: bool) -> Vec<Vec<u64>> {
    let words = LAYOUT.size() as usize / 8;
    let slot = |j: usize| LAYOUT.slot_off[j] as usize / 8;
    (0..m)
        .map(|i| {
            let mut r = vec![0u64; words];
            r[0] = [i64::MIN, -1, 0, 5, i64::MAX][rng.below(5) as usize] as u64;
            r[1] = (i as u64) << 8;
            let (a, b, c) = (pick(rng, big), pick(rng, big), pick(rng, big));
            let narrow = |s: Sum| match s {
                Ok(v) => [v as u64, (v as u128 >> 64) as u64],
                Err(code) => [code, NUM_SENT_HI as u64],
            };
            [r[slot(0)], r[slot(0) + 1]] = narrow(a);
            [r[slot(1)], r[slot(1) + 1], r[slot(1) + 2], r[slot(1) + 3]] = match b {
                Ok(v) => {
                    let hi = v >> 127; // the i256's sign extension
                    [v as u64, (v as u128 >> 64) as u64, hi as u64, hi as u64]
                }
                Err(code) => [code, 0, 0, NUM_SENT_HI as u64],
            };
            let c = c.unwrap_or(9); // sum(int8) has no special: an i128
            [r[slot(2)], r[slot(2) + 1]] = [c as u64, (c as u128 >> 64) as u64];
            r[slot(3)] = rng.below(3);
            // a NULL sum in about one record of six per slot (the count is never NULL)
            let bits = (0..3).fold(0b1000u8, |b, j| if rng.below(6) == 0 { b } else { b | 1 << j });
            set_valid(&mut r, bits);
            r
        })
        .collect()
}

fn set_valid(r: &mut [u64], bits: u8) {
    let (o, _) = LAYOUT.valid_bit(0);
    let bytes = r.as_mut_ptr() as *mut u8;
    // SAFETY: the validity byte lies inside the record's `size()` bytes.
    unsafe { *bytes.add(o) = bits };
}

/// The records' indexes in the order `agg_sort` (or the generic seam) leaves them, and the call's return.
fn sorted(recs: &mut [Vec<u64>], words: &[u64], bound: Option<usize>, generic: bool, ties: bool) -> (Vec<usize>, i64) {
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let all: Vec<*const u8> = ptrs.clone();
    let m = ptrs.len();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: m as u64, kept: m as u64 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 2 << FORM_NULLKEY_SHIFT | bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT) | if ties { FORM_TIES } else { 0 };
    desc[DESC_NSORT / 8] = words.len() as u64;
    desc[DESC_SORT / 8..DESC_SORT / 8 + words.len()].copy_from_slice(words);
    // SAFETY: a ctx is plain words; zero is its closed state (no text word reads a site).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    let (stp, dp) = (&mut st as *mut EmitState as *mut u8, desc.as_ptr() as *const u8);
    // SAFETY: `st` holds `m` live records of `LAYOUT`, `desc` a descriptor of it.
    let r = unsafe { if generic { agg_sort_generic(&mut ctx, stp, dp) } else { agg_sort(&mut ctx as *mut _ as u64, stp as u64, dp as u64) } };
    (ptrs.iter().map(|p| all.iter().position(|q| q == p).expect("a record")).collect(), r)
}

fn word(rng: &mut Rng) -> u64 {
    let base = match rng.below(6) {
        0 | 1 => SK_SLOT,     // sum(numeric)
        2 => SK_SLOT | 1,     // the wide sum
        3 => SK_SLOT | 2,     // sum(int8)
        4 => SK_SLOT | 3,     // count
        _ => 0,               // the int8 key
    };
    base | if rng.below(2) == 0 { SK_DESC } else { 0 } | if rng.below(2) == 0 { SK_NULLS_FIRST } else { 0 }
}

#[test]
fn numeric_sum_rows_order_as_the_comparator() {
    let mut rng = Rng(0x3141_D314_1D31_41D3);
    for case in 0..800 {
        let m = 1 + rng.below(120) as usize;
        let big = case % 4 == 3;
        let mut recs = records(&mut rng, m, big);
        let nw = 1 + rng.below(3) as usize;
        let mut words: Vec<u64> = (0..nw).map(|_| word(&mut rng)).collect();
        words[0] |= SK_SLOT; // a sum or the count leads
        let want = sorted(&mut recs, &words, None, true, false).0;
        let (got, r) = sorted(&mut recs, &words, None, false, false);
        assert_eq!(r, 0);
        assert_eq!(got, want, "case {case}: words {words:x?}");
        for n in [0, 1, 2, m / 2, m.saturating_sub(1), m, m + 3] {
            let cut = sorted(&mut recs, &words, Some(n), false, false).0;
            let k = n.min(m);
            assert_eq!(cut[..k], want[..k], "case {case}: bound {n}, words {words:x?}");
            let (mut rest, mut rest_want) = (cut[k..].to_vec(), want[k..].to_vec());
            rest.sort_unstable();
            rest_want.sort_unstable();
            assert_eq!(rest, rest_want, "case {case}: bound {n}: the records past the cut are the rest");
            if n > 0 && n <= m {
                // WITH TIES: the lead it returns is the whole sort's first records
                let (lead, k) = sorted(&mut recs, &words, Some(n), false, true);
                let k = k as usize;
                assert!(k >= n && k <= m, "case {case}: ties bound {n}: lead {k}");
                assert_eq!(lead[..k], want[..k], "case {case}: ties bound {n}, words {words:x?}");
            }
        }
    }
}

/// numeric_cmp's order over one sum(numeric) word: -Infinity, the mantissas, +Infinity, NaN (DESC the reverse), the
/// OVERFLOW sentinel first in either direction, a NULL where NULLS FIRST / LAST puts it.
#[test]
fn numeric_sum_rows_are_numeric_cmp_order() {
    let words = LAYOUT.size() as usize / 8;
    let s0 = LAYOUT.slot_off[0] as usize / 8;
    let rec = |i: usize, s: Option<Sum>| {
        let mut r = vec![0u64; words];
        r[1] = (i as u64) << 8;
        [r[s0], r[s0 + 1]] = match s {
            Some(Ok(v)) => [v as u64, (v as u128 >> 64) as u64],
            Some(Err(code)) => [code, NUM_SENT_HI as u64],
            None => [0, 0],
        };
        set_valid(&mut r, if s.is_some() { 0b1111 } else { 0b1110 });
        r
    };
    let input = [Some(Err(1)), Some(Ok(5)), None, Some(Err(3)), Some(Ok(-(1i128 << 61))), Some(Err(4)), Some(Err(2)), Some(Ok(0))];
    let mut recs: Vec<Vec<u64>> = input.iter().enumerate().map(|(i, &s)| rec(i, s)).collect();
    let by = |recs: &mut Vec<Vec<u64>>, w: u64| sorted(recs, &[w], None, false, false).0;
    assert_eq!(by(&mut recs, SK_SLOT), vec![5, 3, 4, 7, 1, 6, 0, 2]);
    assert_eq!(by(&mut recs, SK_SLOT | SK_NULLS_FIRST), vec![5, 2, 3, 4, 7, 1, 6, 0]);
    assert_eq!(by(&mut recs, SK_SLOT | SK_DESC), vec![5, 0, 6, 1, 7, 4, 3, 2]);
    assert_eq!(by(&mut recs, SK_SLOT | SK_DESC | SK_NULLS_FIRST), vec![5, 2, 0, 6, 1, 7, 4, 3]);
}
