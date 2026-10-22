//! V4R-190 (M3-MERGE): `agg_sort` orders records whose sort words carry no average and no text through word rows
//! (`decorate`) instead of the generic comparator, and orders only the first `n` under a bound in the form word
//! (`FORM_BOUND_SHIFT`). Both must give the generic comparator's order: every record for a whole sort, the first `n` for a
//! bounded one (the key words end every comparison, so the order is total and the first `n` are the same records). Mixed
//! sort words: key words (an int2 / int4 key as its sign-extended I32 word, an int8 key with i64::MIN / MAX, a float8
//! key with -Inf / -0 / +0 / NaN / +Inf, each NULL-flagged), count / sum / min slots, a float sum, and an average (the
//! fallback), each ASC / DESC and NULLS FIRST / LAST, ties on the leading word (the cut falls inside a tie). A numeric
//! final or key never reaches agg_sort as a word (runs/topk_groups.rs `sort_words` refuses a numeric final; there is no
//! numeric key kind), so no numeric word is decorated.
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, LAYOUT_WORDS};
use gravity_abi::Ctx;
use gravity_kernels::host_abi::{agg_sort, agg_sort_generic, float_order, EmitState, DESC_BYTES, DESC_FORM, DESC_LAYOUT, DESC_NSORT, DESC_SORT, FORM_BOUND_SHIFT, FORM_NULLKEY_SHIFT, FORM_TIES, SK_DESC, SK_FLOAT, SK_NULLS_FIRST, SK_SLOT};

/// Keys: an int2 / int4 key (word 0, sign-extended), an int8 key (word 1), a float8 key (word 2), the NULL-flags word
/// (word 3: bit k = key k NULL; its high bits the record's index, so the key rows are unique).
/// Slots: 0 count, 1 sum(int8), 2 min(int8) (NULL in some records: the validity word), 3 sum(float8), 4 avg(int) (the
/// generic fallback).
const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I32, KeyKind::I64, KeyKind::F64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64, SlotKind::MinI64, SlotKind::SumF64, SlotKind::AvgI128]);

/// A small deterministic generator (xorshift64*).
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

/// `m` records with few distinct values per word (ties), the extremes of each kind among them.
fn records(rng: &mut Rng, m: usize) -> Vec<Vec<u64>> {
    let words = LAYOUT.size() as usize / 8;
    let floats = [f64::NEG_INFINITY, -2.5, -0.0, 0.0, 1.0, f64::NAN, f64::INFINITY];
    let i16s = [i16::MIN as i64, -300, -1, 0, 7, i16::MAX as i64, i32::MIN as i64, i32::MAX as i64];
    let i64s = [i64::MIN, -5_000_000_000, -1, 0, 1, i64::MAX];
    (0..m)
        .map(|i| {
            let mut r = vec![0u64; words];
            let nulls: Vec<bool> = (0..3).map(|_| rng.below(7) == 0).collect();
            r[0] = if nulls[0] { 0 } else { i16s[rng.below(i16s.len() as u64) as usize] as u64 };
            r[1] = if nulls[1] { 0 } else { i64s[rng.below(i64s.len() as u64) as usize] as u64 };
            r[2] = if nulls[2] { 0 } else { floats[rng.below(floats.len() as u64) as usize].to_bits() };
            r[3] = nulls[0] as u64 | (nulls[1] as u64) << 1 | (nulls[2] as u64) << 2 | (i as u64) << 8;
            let slot = |j: usize| LAYOUT.slot_off[j] as usize / 8;
            r[slot(0)] = rng.below(4);
            r[slot(1)] = i64s[rng.below(i64s.len() as u64) as usize] as u64;
            r[slot(2)] = i16s[rng.below(i16s.len() as u64) as usize] as u64;
            r[slot(3)] = floats[rng.below(floats.len() as u64) as usize].to_bits();
            let a = slot(4);
            let (sum, n) = (rng.below(9) as i128 - 4, rng.below(3) as i64);
            let s = sum as u128;
            (r[a], r[a + 1], r[a + 2]) = (s as u64, (s >> 64) as u64, n as u64);
            // every slot valid but the min in about one record of five
            let valid = if rng.below(5) == 0 { 0b11011u8 } else { 0b11111 };
            set_valid(&mut r, valid);
            r
        })
        .collect()
}

/// The record's validity byte (slot j's bit j: `RecordLayout::valid_bit`).
fn set_valid(r: &mut [u64], bits: u8) {
    let (o, _) = LAYOUT.valid_bit(0);
    let bytes = r.as_mut_ptr() as *mut u8;
    // SAFETY: the validity byte lies inside the record's `size()` bytes.
    unsafe { *bytes.add(o) = bits };
}

/// The records' indexes in the order `agg_sort` (or, `generic`, the generic seam) leaves them, under sort words `words` and bound `n`.
fn order(recs: &mut [Vec<u64>], words: &[u64], bound: Option<usize>, generic: bool) -> Vec<usize> {
    let (o, r) = sorted(recs, words, bound, generic, false);
    assert_eq!(r, 0);
    o
}

/// `order`, and with `ties` a bound WITH TIES (`FORM_TIES`): the order and `agg_sort`'s return.
fn sorted(recs: &mut [Vec<u64>], words: &[u64], bound: Option<usize>, generic: bool, ties: bool) -> (Vec<usize>, i64) {
    let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
    let all: Vec<*const u8> = ptrs.clone();
    let m = ptrs.len();
    let mut st = EmitState { ptrs: ptrs.as_mut_ptr(), len: m as u64, kept: m as u64 };
    let mut desc = vec![0u64; DESC_BYTES as usize / 8];
    let lw: [u64; LAYOUT_WORDS] = LAYOUT.words();
    desc[DESC_LAYOUT / 8..DESC_LAYOUT / 8 + LAYOUT_WORDS].copy_from_slice(&lw);
    desc[DESC_FORM / 8] = 4 << FORM_NULLKEY_SHIFT | bound.map_or(0, |n| (n as u64 + 1) << FORM_BOUND_SHIFT) | if ties { FORM_TIES } else { 0 };
    desc[DESC_NSORT / 8] = words.len() as u64;
    desc[DESC_SORT / 8..DESC_SORT / 8 + words.len()].copy_from_slice(words);
    // SAFETY: a ctx is plain words; zero is its closed state (no text word reads a site).
    let mut ctx: Ctx = unsafe { core::mem::zeroed() };
    let (stp, dp) = (&mut st as *mut EmitState as *mut u8, desc.as_ptr() as *const u8);
    // SAFETY: `st` holds `m` live records of `LAYOUT`, `desc` a descriptor of it.
    let r = unsafe { if generic { agg_sort_generic(&mut ctx, stp, dp) } else { agg_sort(&mut ctx as *mut _ as u64, stp as u64, dp as u64) } };
    (ptrs.iter().map(|p| all.iter().position(|q| q == p).expect("a record")).collect(), r)
}

/// A sort word of kind `kind` (a key word, its NULL bit its own index in flags word 3; or a slot) with `DESC` / `NULLS FIRST`.
fn word(kind: u64, rng: &mut Rng) -> u64 {
    let base = match kind {
        0 => 0,                 // the int2 / int4 key
        1 => 1,                 // the int8 key
        2 => 2 | SK_FLOAT,      // the float8 key
        3 => SK_SLOT,           // count
        4 => SK_SLOT | 1,       // sum(int8)
        5 => SK_SLOT | 2,       // min(int8)
        6 => SK_SLOT | 3 | SK_FLOAT,
        _ => SK_SLOT | 4,       // avg: the generic fallback
    };
    base | if rng.below(2) == 0 { SK_DESC } else { 0 } | if rng.below(2) == 0 { SK_NULLS_FIRST } else { 0 }
}

#[test]
fn decorated_order_is_the_generic_order() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..600 {
        let m = 1 + rng.below(90) as usize;
        let mut recs = records(&mut rng, m);
        let nw = 1 + rng.below(3) as usize;
        let words: Vec<u64> = (0..nw).map(|_| { let k = rng.below(8); word(k, &mut rng) }).collect();
        let want = order(&mut recs, &words, None, true);
        let got = order(&mut recs, &words, None, false);
        assert_eq!(got, want, "case {case}: words {words:x?}");
        // a bound inside the records (a cut in a tie of the leading word too) and past them
        for n in [0, 1, 2, m / 2, m.saturating_sub(1), m, m + 3] {
            let cut = order(&mut recs, &words, Some(n), false);
            let k = n.min(m);
            assert_eq!(cut[..k], want[..k], "case {case}: bound {n}, words {words:x?}");
            let mut rest = cut[k..].to_vec();
            let mut rest_want = want[k..].to_vec();
            rest.sort_unstable();
            rest_want.sort_unstable();
            assert_eq!(rest, rest_want, "case {case}: bound {n}: the records past the cut are the rest");
        }
    }
}

/// The decorated order is PostgreSQL's for each word kind it accepts, not only the comparator's: a sign-extended int4 /
/// int2 word and an int8 word with their negatives, a float8 word with -Inf first, -0 and +0 tied (then by the key rows),
/// NaN last and above +Inf; DESC the reverse.
#[test]
fn decorated_order_is_postgres_order_per_kind() {
    let words = LAYOUT.size() as usize / 8;
    let rec = |i: usize, a: i64, b: i64, f: f64| {
        let mut r = vec![0u64; words];
        (r[0], r[1], r[2], r[3]) = (a as u64, b as u64, f.to_bits(), (i as u64) << 8);
        set_valid(&mut r, 0b11111);
        r
    };
    let input = [(-300i64, i64::MIN, f64::NAN), (7, -1, 0.0), (i16::MIN as i64, i64::MAX, f64::NEG_INFINITY), (-1, 0, -0.0), (i32::MAX as i64, -5_000_000_000, f64::INFINITY), (0, 1, 1.0)];
    let mut recs: Vec<Vec<u64>> = input.iter().enumerate().map(|(i, &(a, b, f))| rec(i, a, b, f)).collect();
    let by = |recs: &mut Vec<Vec<u64>>, w: u64| order(recs, &[w], None, false);
    // int4 ASC: i16::MIN, -300, -1, 0, 7, i32::MAX
    assert_eq!(by(&mut recs, 0), vec![2, 0, 3, 5, 1, 4]);
    // int8 ASC: MIN, -5e9, -1, 0, 1, MAX; DESC the reverse
    assert_eq!(by(&mut recs, 1), vec![0, 4, 1, 3, 5, 2]);
    assert_eq!(by(&mut recs, 1 | SK_DESC), vec![2, 5, 3, 1, 4, 0]);
    // float8 ASC: -Inf, then -0 (record 3) and +0 (record 1) tied and ordered by the key rows (record 1's int4 7 > -1),
    // 1, +Inf, NaN; DESC: NaN first, the tie still by the key rows
    assert_eq!(by(&mut recs, 2 | SK_FLOAT), vec![2, 3, 1, 5, 4, 0]);
    assert_eq!(by(&mut recs, 2 | SK_FLOAT | SK_DESC), vec![0, 4, 5, 3, 1, 2]);
}

/// KEYED-05: the generic comparator (an average sort word) honours the bound too: the first `n` records are the whole
/// sort's first `n` in order, the rest are the remaining records in any order.
#[test]
fn generic_bound_orders_the_first_n() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for case in 0..300 {
        let m = 1 + rng.below(120) as usize;
        let mut recs = records(&mut rng, m);
        let lead = word(7, &mut rng); // avg: the generic path
        let words = [lead, word(rng.below(7), &mut rng)];
        let want = order(&mut recs, &words, None, true);
        for n in [0, 1, 3, m / 2, m.saturating_sub(1), m, m + 2] {
            let cut = order(&mut recs, &words, Some(n), false);
            let k = n.min(m);
            assert_eq!(cut[..k], want[..k], "case {case}: bound {n}, words {words:x?}");
            let (mut rest, mut rest_want) = (cut[k..].to_vec(), want[k..].to_vec());
            rest.sort_unstable();
            rest_want.sort_unstable();
            assert_eq!(rest, rest_want, "case {case}: bound {n}");
        }
    }
}

/// RUNS-18 (`FORM_TIES`): a bound WITH TIES orders the first `n`, every record whose sort words equal the `n`-th's, and the
/// least record past them (the row a Limit WITH TIES stops at), and returns how many it ordered: the whole sort's first
/// ones. A bound of 0 orders nothing, a bound past the records orders them all; the comparator's road (an average word)
/// orders every record.
#[test]
fn ties_bound_orders_the_ties_and_the_next_record() {
    let words = LAYOUT.size() as usize / 8;
    let rec = |i: usize, b: i64| {
        let mut r = vec![0u64; words];
        (r[0], r[1], r[2], r[3]) = (i as u64, b as u64, 0f64.to_bits(), (i as u64) << 8);
        set_valid(&mut r, 0b11111);
        r
    };
    // the int8 key word: three 5s, four 7s, one 9, two 11s
    let vals = [7i64, 5, 11, 7, 9, 5, 7, 11, 5, 7];
    let mut recs: Vec<Vec<u64>> = vals.iter().enumerate().map(|(i, &b)| rec(i, b)).collect();
    for (w, cases) in [(1u64, &[(0usize, 0i64), (1, 4), (2, 4), (3, 4), (4, 8), (7, 8), (8, 9), (9, 10), (10, 10), (13, 10)][..]), (1 | SK_DESC, &[(1, 3), (2, 3), (3, 4), (4, 8), (8, 10)][..])] {
        let want = order(&mut recs, &[w], None, true);
        for &(n, k) in cases {
            let (got, r) = sorted(&mut recs, &[w], Some(n), false, true);
            assert_eq!(r, k, "word {w:x}: bound {n} orders {k}");
            assert_eq!(got[..k as usize], want[..k as usize], "word {w:x}: bound {n}");
        }
    }
    let avg = [SK_SLOT | 4, 1];
    let want = order(&mut recs, &avg, None, true);
    assert_eq!(sorted(&mut recs, &avg, Some(1), false, true), (want, recs.len() as i64), "the comparator's road orders every record");
    // random records and words: the lead is the whole sort's, past the bound whenever records are left
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..400 {
        let m = 1 + rng.below(90) as usize;
        let mut recs = records(&mut rng, m);
        let nw = 1 + rng.below(3) as usize;
        let words: Vec<u64> = (0..nw).map(|_| { let k = rng.below(7); word(k, &mut rng) }).collect();
        let want = order(&mut recs, &words, None, true);
        for n in [1, 2, m / 2, m.saturating_sub(1), m] {
            let (got, r) = sorted(&mut recs, &words, Some(n), false, true);
            let r = r as usize;
            assert!(r >= (n + 1).min(m) && r <= m, "case {case}: bound {n} ordered {r} of {m}");
            assert_eq!(got[..r], want[..r], "case {case}: bound {n}, words {words:x?}");
        }
    }
}

/// Record `r`'s cell under plain sort word `w` (kinds 0..=6 of `word`): `None` for a NULL (a key's flag bit in word 3, a
/// slot's validity bit; a count is never NULL), a float by its total-order word (-0 as +0, every NaN alike).
fn cell(r: &[u64], w: u64) -> Option<i64> {
    let j = (w & 0xff) as usize;
    let (v, ok) = if w & SK_SLOT == 0 {
        (r[j], r[3] >> j & 1 == 0)
    } else {
        let (o, sh) = LAYOUT.valid_bit(j);
        // SAFETY: the validity byte lies inside the record's `size()` bytes.
        let bits = unsafe { *(r.as_ptr() as *const u8).add(o) };
        (r[LAYOUT.slot_off[j] as usize / 8], j == 0 || bits >> sh & 1 != 0)
    };
    ok.then(|| if w & SK_FLOAT != 0 { float_order(f64::from_bits(v)) } else { v as i64 })
}

/// NEW-35: past `3n` records a bound runs through a buffer of `2n` rows (kernels `select_decorated`). Its first `n` are the
/// whole sort's in order and the rest are the remaining records, whatever order the records arrive in: as made, the sort's
/// own order and its reverse (every record then enters the buffer). WITH TIES the lead is exactly the first `n`, every
/// record whose sort words equal the `n`-th's and the least record past them.
#[test]
fn a_bound_past_twice_n_records_is_the_whole_sorts() {
    let mut rng = Rng(0x5851_F42D_4C95_7F2D);
    for case in 0..120 {
        let m = 5 + rng.below(600) as usize;
        let recs = records(&mut rng, m);
        let nw = 1 + rng.below(3) as usize;
        let words: Vec<u64> = (0..nw).map(|_| { let k = rng.below(7); word(k, &mut rng) }).collect();
        let want = order(&mut recs.clone(), &words, None, true);
        let arrivals = [(0..m).collect::<Vec<usize>>(), want.clone(), want.iter().rev().copied().collect()];
        for (a, arr) in arrivals.iter().enumerate() {
            let mut input: Vec<Vec<u64>> = arr.iter().map(|&i| recs[i].clone()).collect();
            for n in [1, 2, 3, 7, (m - 1) / 2, (m - 1) / 3, m / 3, m / 4, m / 5] {
                if n == 0 || 2 * n >= m {
                    continue;
                }
                let cut: Vec<usize> = order(&mut input, &words, Some(n), false).iter().map(|&p| arr[p]).collect();
                assert_eq!(cut[..n], want[..n], "case {case}: arrival {a}, bound {n}, words {words:x?}");
                let (mut rest, mut rest_want) = (cut[n..].to_vec(), want[n..].to_vec());
                rest.sort_unstable();
                rest_want.sort_unstable();
                assert_eq!(rest, rest_want, "case {case}: arrival {a}, bound {n}: the records past the cut are the rest");
                let last: Vec<Option<i64>> = words.iter().map(|&w| cell(&recs[want[n - 1]], w)).collect();
                let tied = want[n..].iter().take_while(|&&i| words.iter().zip(&last).all(|(&w, l)| cell(&recs[i], w) == *l)).count();
                let k = (n + tied + 1).min(m);
                let (got, r) = sorted(&mut input, &words, Some(n), false, true);
                let got: Vec<usize> = got.iter().map(|&p| arr[p]).collect();
                assert_eq!(r as usize, k, "case {case}: arrival {a}, bound {n} WITH TIES, words {words:x?}");
                assert_eq!(got[..k], want[..k], "case {case}: arrival {a}, bound {n} WITH TIES, words {words:x?}");
                let (mut rest, mut rest_want) = (got[k..].to_vec(), want[k..].to_vec());
                rest.sort_unstable();
                rest_want.sort_unstable();
                assert_eq!(rest, rest_want, "case {case}: arrival {a}, bound {n} WITH TIES: the rest");
            }
        }
    }
}
