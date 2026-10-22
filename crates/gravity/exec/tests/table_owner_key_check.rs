//! The count form's key check (`OwnerDesc::check`): chains of integer `+`, `-`, `*` with constants, `-` and narrowing
//! casts (NEW-46) over several group keys, every step at one width. The owner raises the width's 22003 exactly when some
//! folded group's chain leaves the width at some step, as PostgreSQL projecting every group would: the bind's per-key
//! intervals against a step-by-step evaluation, over chains and constants at the edges of the widths; a NULL key raises
//! nothing.
use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind};
use gravity_abi::status::{raise, raised};
use gravity_exec::table::owner::bind;
use gravity_program::{CheckStep, KeyCheck, OwnerDesc, CHECK_ADD, CHECK_CAST, CHECK_KEY_NULLABLE, CHECK_MUL, CHECK_NEG, CHECK_RSUB, CHECK_SUB, KEY_CHECK_KEYS};

/// A chain: its key (0 the int4 / int2 key, 1 the int8 key) and its steps `(op, constant width, constant)`.
type Chain = (usize, Vec<(u8, u8, i64)>);

fn range(w: u8) -> (i128, i128) {
    match w {
        2 => (i16::MIN as i128, i16::MAX as i128),
        4 => (i32::MIN as i128, i32::MAX as i128),
        _ => (i64::MIN as i128, i64::MAX as i128),
    }
}

/// Whether PostgreSQL's projection raises on the groups `keys`: some chain leaves `width` at some step.
fn raises(width: u8, chains: &[Chain], keys: &[(i64, i64)]) -> bool {
    let (min, max) = range(width);
    keys.iter().any(|&(a, b)| {
        chains.iter().any(|(key, steps)| {
            let mut v = if *key == 0 { a as i128 } else { b as i128 };
            steps.iter().any(|&(op, _, c)| {
                let c = c as i128;
                v = match op {
                    CHECK_ADD => v + c,
                    CHECK_SUB => v - c,
                    CHECK_RSUB => c - v,
                    CHECK_NEG => -v,
                    CHECK_CAST => v,
                    _ => v * c,
                };
                v < min || v > max
            })
        })
    })
}

/// The owner's answer over the groups `keys`: `Some(code)` raised, `None` kept.
fn owner(width: u8, chains: &[Chain], keys: &[(i64, i64)]) -> Option<i64> {
    let groups: Vec<[u64; 3]> = keys.iter().map(|&(a, b)| [a as u64, b as u64, 0]).collect();
    owner_flagged(width, chains, &groups, None)
}

/// `owner` over groups `[a, b, flags]`; with `nulls` (each key's `CHECK_KEY_NULLABLE` mark), the record carries the NULL
/// flags word after its two keys (bit 0: a NULL, bit 1: b NULL) and the descriptor reads it (`tie_flags`).
fn owner_flagged(width: u8, chains: &[Chain], groups: &[[u64; 3]], nulls: Option<[bool; 2]>) -> Option<i64> {
    let flagged = [KeyKind::I32, KeyKind::I64, KeyKind::I64];
    let layout = RecordLayout::new(if nulls.is_some() { &flagged } else { &flagged[..2] }, &[SlotKind::Count]);
    let mut words = Vec::new();
    let mut all = Vec::new();
    for (key, steps) in chains {
        let mark = if nulls.is_some_and(|n| n[*key]) { CHECK_KEY_NULLABLE } else { 0 };
        for (n, &(op, cwidth, c)) in steps.iter().enumerate() {
            all.push(CheckStep { key: if n == 0 { (*key as u8 + 1) | mark } else { 0 }, op, cwidth, word: words.len() as u16 });
            words.push(c as u64);
        }
    }
    let arena = gravity_program::Arena::new(1 << 12);
    let d = OwnerDesc::count_topk(layout, 0, 2).key_check(KeyCheck::in_arena(arena.handle(), width, &all));
    let d = if nulls.is_some() { d.tie_flags(2) } else { d };
    let fns = bind(&d, &mut words, 4, None);
    let cap = 1u32 << 6;
    let limit = HashAgg::bytes(&layout, cap);
    let mut tbuf = vec![0u64; limit.div_ceil(8) as usize];
    let t = unsafe { HashAgg::at(tbuf.as_mut_ptr() as *mut u8, &layout, cap, limit) };
    for g in groups {
        t.find_or_insert(&g[..layout.nkeys()]).expect("room").add_count(0, 1);
    }
    let bytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * limit as usize;
    let mut obuf = vec![0u64; bytes.div_ceil(8)];
    let o = obuf.as_mut_ptr() as *mut u8;
    unsafe {
        assert!((fns.open)(fns.params.as_ptr(), o, bytes) > 0, "the owner opens");
        let r = (fns.reduce)(fns.params.as_ptr(), o, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1);
        (r != 0).then_some(r)
    }
}

fn code(width: u8) -> i64 {
    raised(raise::bit(match width {
        2 => raise::INT2_RANGE,
        4 => raise::INT4_RANGE,
        _ => raise::INT8_RANGE,
    }))
}

#[test]
fn offset_chains_raise_only_past_the_width() {
    let offsets: Vec<Chain> = (1..=3).map(|c| (0, vec![(CHECK_SUB, 4, c)])).collect();
    assert_eq!(owner(4, &offsets, &[(7, 0), (i32::MIN as i64 + 3, 0)]), None, "a - 3 fits at INT_MIN + 3");
    assert_eq!(owner(4, &offsets, &[(7, 0), (i32::MIN as i64 + 2, 0)]), Some(code(4)), "a - 3 leaves int4 at INT_MIN + 2");
    let nested: Vec<Chain> = vec![(0, vec![(CHECK_ADD, 4, 1), (CHECK_MUL, 4, 2)])];
    assert_eq!(owner(4, &nested, &[((1 << 30) - 2, 0)]), None, "(a + 1) * 2 = 2^31 - 2 fits int4");
    assert_eq!(owner(4, &nested, &[((1 << 30) - 1, 0)]), Some(code(4)), "(a + 1) * 2 = 2^31 leaves int4 at the second step");
    let wide: Vec<Chain> = vec![(0, vec![(CHECK_ADD, 8, 1), (CHECK_MUL, 8, 2)]), (1, vec![(CHECK_RSUB, 8, 5)])];
    assert_eq!(owner(8, &wide, &[((1 << 30) - 1, i64::MIN + 6)]), None, "(a + 1::int8) * 2 of an int4 key, and 5 - b = INT8_MAX");
    assert_eq!(owner(8, &wide, &[((1 << 30) - 1, i64::MIN + 6), (0, i64::MIN + 5)]), Some(code(8)), "5 - b leaves int8 on the second key");
}

/// Every chain shape over both keys at each width, constants and keys at the edges: the owner raises where a step-by-step
/// evaluation leaves the width and nowhere else.
#[test]
fn the_owner_raises_exactly_where_a_step_leaves_the_width() {
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let edges = |w: u8| -> Vec<i64> {
        let (min, max) = range(w);
        let (min, max) = (min as i64, max as i64);
        vec![0, 1, -1, 2, -2, 3, 7, -7, 1000, -1000, max, min, max - 1, min + 1, max / 2, min / 2, max / 3 + 1, min / 3 - 1, max / 1000, min / 1000]
    };
    let (mut raised_n, mut kept_n) = (0, 0);
    for _ in 0..20_000 {
        let width = [2u8, 4, 8][rnd() as usize % 3];
        let keyw = |key: usize| if key == 1 { 8 } else if width == 2 { 2 } else { 4 };
        let mut chains: Vec<Chain> = Vec::new();
        for _ in 0..1 + rnd() % 3 {
            // NEW-46: the int8 key under any width when its chain starts with a narrowing cast to the width (`k8::int4 + 5`)
            let cast_first = width < 8 && rnd() % 3 == 0;
            let key = if width == 8 || cast_first { rnd() as usize % 2 } else { 0 };
            let n = (1 + rnd() % 3) as usize;
            let steps = (0..n)
                .map(|i| {
                    let op = if cast_first && i == 0 { CHECK_CAST } else { [CHECK_ADD, CHECK_SUB, CHECK_RSUB, CHECK_MUL, CHECK_NEG, CHECK_CAST][rnd() as usize % 6] };
                    let ws: Vec<u8> = [2u8, 4, 8].into_iter().filter(|&w| w <= width).collect();
                    let cw = ws[rnd() as usize % ws.len()];
                    let e = edges(cw);
                    (op, cw, e[rnd() as usize % e.len()])
                })
                .collect();
            chains.push((key, steps));
        }
        let ka = edges(keyw(0));
        let kb = edges(8);
        let mut keys: Vec<(i64, i64)> = (0..1 + rnd() % 4).map(|_| (ka[rnd() as usize % ka.len()], if chains.iter().any(|c| c.0 == 1) { kb[rnd() as usize % kb.len()] } else { 0 })).collect();
        keys.sort_unstable();
        keys.dedup();
        let want = raises(width, &chains, &keys);
        let got = owner(width, &chains, &keys);
        assert_eq!(got, want.then(|| code(width)), "width {width} chains {chains:?} keys {keys:?}");
        if want { raised_n += 1 } else { kept_n += 1 }
    }
    assert!(raised_n > 1000 && kept_n > 1000, "both answers drawn: {raised_n} raised, {kept_n} kept");
}

/// NEW-46: `-b` and `b::int4 + 5` (a narrowing cast) raise at their width's edges, as PostgreSQL's int8um / int84 + int4pl.
#[test]
fn a_minus_and_a_narrowing_cast_raise_at_their_edges() {
    let neg: Vec<Chain> = vec![(1, vec![(CHECK_NEG, 8, 0)])];
    assert_eq!(owner(8, &neg, &[(0, i64::MIN + 1), (0, i64::MAX)]), None, "-b fits from INT8_MIN + 1");
    assert_eq!(owner(8, &neg, &[(0, 7), (0, i64::MIN)]), Some(code(8)), "-INT8_MIN leaves int8");
    let cast: Vec<Chain> = vec![(1, vec![(CHECK_CAST, 4, 0), (CHECK_ADD, 4, 5)])];
    assert_eq!(owner(4, &cast, &[(0, i32::MAX as i64 - 5), (0, i32::MIN as i64)]), None, "b::int4 + 5 fits up to INT4_MAX - 5");
    assert_eq!(owner(4, &cast, &[(0, i32::MAX as i64 - 4)]), Some(code(4)), "b::int4 + 5 leaves int4 at the add");
    assert_eq!(owner(4, &cast, &[(0, i32::MIN as i64 - 1)]), Some(code(4)), "b::int4 leaves int4 at the cast");
    let small: Vec<Chain> = vec![(0, vec![(CHECK_CAST, 2, 0), (CHECK_NEG, 2, 0)])];
    assert_eq!(owner(2, &small, &[(i16::MIN as i64 + 1, 0)]), None, "-(a::int2) at INT2_MIN + 1");
    assert_eq!(owner(2, &small, &[(i16::MIN as i64, 0)]), Some(code(2)), "-(a::int2) leaves int2 at INT2_MIN");
}

/// NEW-46: a nullable key's NULL group (image 0, its flag set) raises nothing where its zero would, beside a non-NULL zero
/// that raises; an unmarked key's flag is never read.
#[test]
fn a_null_key_raises_nothing() {
    // b - INT8_MAX - 10 leaves int8 at b = 0
    let c: Vec<Chain> = vec![(1, vec![(CHECK_SUB, 8, i64::MAX), (CHECK_SUB, 8, 10)])];
    let null_b = [7u64, 0, 0b10];
    let zero_b = [8u64, 0, 0];
    assert_eq!(owner_flagged(8, &c, &[null_b], Some([false, true])), None, "the NULL b group raises nothing");
    assert_eq!(owner_flagged(8, &c, &[null_b, zero_b], Some([false, true])), Some(code(8)), "the zero b group raises");
    assert_eq!(owner_flagged(8, &c, &[null_b], Some([false, false])), Some(code(8)), "an unmarked key reads no flag");
    // a::int2 + INT2_MAX + 1 leaves int2 at a = 0 and fits at a = -1
    let a: Vec<Chain> = vec![(0, vec![(CHECK_CAST, 2, 0), (CHECK_ADD, 2, i16::MAX as i64), (CHECK_ADD, 2, 1)])];
    let null_a = [0u64, 3, 0b01];
    assert_eq!(owner_flagged(2, &a, &[null_a, [(-1i64) as u64, 4, 0]], Some([true, false])), None, "a NULL a and a = -1 fit");
    assert_eq!(owner_flagged(2, &a, &[null_a, [0, 4, 0]], Some([true, false])), Some(code(2)), "a = 0 leaves int2");
}

/// D4456: the key check's scans (one key at either width with no NULL flag, each its own loop; several keys, the general
/// loop) read every used record of the table and skip the unused ones: 40 groups inside the width keep, and each of 41
/// groups past it (41 keys, so 41 home slots), added at each insertion position, raises.
#[test]
fn every_used_record_is_checked_wherever_it_sits() {
    let narrow: Vec<Chain> = vec![(0, vec![(CHECK_MUL, 4, 2)])];
    let wide: Vec<Chain> = vec![(1, vec![(CHECK_MUL, 8, 2)])];
    let both: Vec<Chain> = vec![(0, vec![(CHECK_MUL, 4, 2)]), (1, vec![(CHECK_MUL, 8, 2)])];
    let fits: Vec<(i64, i64)> = (0..40).map(|i| (1000 + 7 * i, -1000 - 11 * i)).collect();
    let past4 = |j: i64| ((1i64 << 30) + 1 + 7919 * j, 0i64);
    let past8 = |j: i64| (0i64, (1i64 << 62) + 1 + 7919 * j);
    let cases: [(&str, u8, &Vec<Chain>, &dyn Fn(i64) -> (i64, i64)); 3] =
        [("one int4 key", 4, &narrow, &past4), ("one int8 key", 8, &wide, &past8), ("two keys", 8, &both, &past8)];
    for (name, width, chains, past) in cases {
        assert_eq!(owner(width, chains, &fits), None, "{name}: 40 groups inside the width");
        for j in 0..=fits.len() {
            let bad = past(j as i64);
            assert!(raises(width, chains, &[bad]), "{name}: the reference raises on {bad:?}");
            let mut keys = fits.clone();
            keys.insert(j, bad);
            assert_eq!(owner(width, chains, &keys), Some(code(width)), "{name}: {bad:?} past the width, inserted at {j}");
        }
    }
}

/// A-23: a check over every key of the record's inline tier (`KEY_CHECK_KEYS`), one key's chain twelve steps long: the
/// owner keeps groups whose every chain fits and raises at a group past the width on the last key or at the last step.
#[test]
fn a_check_over_every_inline_key_and_a_long_chain() {
    let kinds = [KeyKind::I64; KEY_CHECK_KEYS];
    let layout = RecordLayout::new(&kinds, &[SlotKind::Count]);
    // key j: k_j + j; key 0 also ((k0 + 1) * 2 - 1) * 2 ... twelve steps, the value growing about 16x
    let long: Vec<(u8, i64)> = vec![(CHECK_ADD, 1), (CHECK_MUL, 2), (CHECK_SUB, 1), (CHECK_MUL, 2), (CHECK_ADD, 3), (CHECK_MUL, 2), (CHECK_SUB, 5), (CHECK_MUL, 2), (CHECK_ADD, 7), (CHECK_RSUB, 11), (CHECK_ADD, 13), (CHECK_NEG, 0)];
    let run = |groups: &[[i64; KEY_CHECK_KEYS]]| -> Option<i64> {
        let mut words: Vec<u64> = Vec::new();
        let mut all = Vec::new();
        for j in 0..KEY_CHECK_KEYS {
            all.push(CheckStep { key: j as u8 + 1, op: CHECK_ADD, cwidth: 8, word: words.len() as u16 });
            words.push(j as u64);
        }
        for (n, &(op, c)) in long.iter().enumerate() {
            all.push(CheckStep { key: if n == 0 { 1 } else { 0 }, op, cwidth: 8, word: words.len() as u16 });
            words.push(c as u64);
        }
        assert!(all.len() > 8 + 8, "past the eight steps a check once held");
        let arena = gravity_program::Arena::new(1 << 12);
        let d = OwnerDesc::count_topk(layout, 0, 2).key_check(KeyCheck::in_arena(arena.handle(), 8, &all));
        let fns = bind(&d, &mut words, 4, None);
        let cap = 1u32 << 6;
        let limit = HashAgg::bytes(&layout, cap);
        let mut tbuf = vec![0u64; limit.div_ceil(8) as usize];
        let t = unsafe { HashAgg::at(tbuf.as_mut_ptr() as *mut u8, &layout, cap, limit) };
        for g in groups {
            let k: Vec<u64> = g.iter().map(|&v| v as u64).collect();
            t.find_or_insert(&k).expect("room").add_count(0, 1);
        }
        let bytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * limit as usize;
        let mut obuf = vec![0u64; bytes.div_ceil(8)];
        let o = obuf.as_mut_ptr() as *mut u8;
        unsafe {
            assert!((fns.open)(fns.params.as_ptr(), o, bytes) > 0, "the owner opens");
            let r = (fns.reduce)(fns.params.as_ptr(), o, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1);
            (r != 0).then_some(r)
        }
    };
    let last = KEY_CHECK_KEYS - 1;
    let mut fits = [0i64; KEY_CHECK_KEYS];
    fits[last] = i64::MAX - last as i64;
    let mut past = fits;
    past[last] += 1;
    assert_eq!(run(&[[1; KEY_CHECK_KEYS], fits]), None, "every chain fits");
    assert_eq!(run(&[[1; KEY_CHECK_KEYS], past]), Some(code(8)), "the last key's chain leaves int8");
    let mut wide = [0i64; KEY_CHECK_KEYS];
    wide[0] = i64::MAX / 8;
    assert_eq!(run(&[[1; KEY_CHECK_KEYS], wide]), Some(code(8)), "the long chain leaves int8 at a late step");
}
