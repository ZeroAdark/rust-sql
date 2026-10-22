//! The LIKE twins' spelling rules (NEW-19: one rule set, two readers). A constant prefix or substring test in another
//! spelling has a LIKE twin with the same answer, NULL and error set under a UTF-8 database and a set, deterministic
//! collation: `starts_with(x, c)` / `x ^@ c` is `x LIKE c%`; an int4 compare of `strpos(x, c)` / `position(c IN x)` with a
//! constant that is a membership test (`textpos_test`) is `x [NOT] LIKE %c%`. Both readers ask here: decompose
//! normalize's `twin_of` rewrites the graph's filters over the plan, and expr percode's `text_search` reads the twin from a
//! program at the sites normalize does not rewrite (an aggregate's FILTER or argument, a projection, a join or subplan
//! qual). Which function is starts_with or a textpos call is a catalog fact (rule R3: a literal OID list lives in a
//! catalog, typesem or normalize home): each reader asks its own home (normalize's `F_STARTS_WITH` / `F_TEXTPOS` over the
//! plan, gravity_fn typesem's `starts_with` / `text_position` over a program; exec's `twin_homes_name_one_list` pins them
//! equal) and hands this module the classes. The guards (the collation, the database encoding, a non-null text constant)
//! stay with each reader's own facts.

/// The twin of a spelling, its function's class read by the caller's home: `starts_with` itself, or an int4 compare `top`
/// of a textpos call (`pos`: whether the call is the compare's left operand) with the constant `k`. `Some((head, tail,
/// negated))`: the twin is `x [NOT] LIKE twin_pattern(c, head, tail)`, NOT when `negated`.
pub fn twin_spelling(starts_with: bool, top: u32, pos: Option<bool>, k: Option<i32>) -> Option<(bool, bool, bool)> {
    if starts_with {
        return Some((false, true, false));
    }
    let found = textpos_test(top, k?, pos?)?;
    Some((true, true, !found))
}

/// What `s op k` says of a strpos result `s`, for an int4 compare whose function is `op` (`k op s` when `!left`):
/// `Some(true)` when it holds exactly for s >= 1 (the needle occurs), `Some(false)` exactly for s = 0, `None` otherwise
/// (a position test, or one answer both ways). Over 1..=i32::MAX the compare steps only next to k, so it is constant
/// there when it agrees at 1, at i32::MAX and at k - 1, k, k + 1 in that range.
pub fn textpos_test(op: u32, k: i32, left: bool) -> Option<bool> {
    let (k, max) = (i64::from(k), i64::from(i32::MAX));
    let at = |s: i64| {
        let (a, b) = if left { (s, k) } else { (k, s) };
        Some(match op { 65 => a == b, 144 => a != b, 66 => a < b, 149 => a <= b, 147 => a > b, 150 => a >= b, _ => return None })
    };
    let found = at(1)?;
    let steady = [max, k - 1, k, k + 1].into_iter().filter(|s| (1..=max).contains(s)).all(|s| at(s) == Some(found));
    (steady && at(0) != Some(found)).then_some(found)
}

/// The twin's LIKE pattern of needle bytes `c`: each `%`, `_` and `\` escaped, `%` before it when `head`, after it when
/// `tail`.
pub fn twin_pattern(c: &[u8], head: bool, tail: bool) -> Vec<u8> {
    // one per rewritten node or recognized site, at build (and per front hit of a twin, exec inputs/front.rs)
    let mut pattern = Vec::with_capacity(c.len() + 2); // alloc: Cold
    if head {
        pattern.push(b'%');
    }
    for &ch in c {
        if matches!(ch, b'%' | b'_' | b'\\') {
            pattern.push(b'\\');
        }
        pattern.push(ch);
    }
    if tail {
        pattern.push(b'%');
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The int4 compares = <> < <= > >= by function.
    const INT4_CMP: [u32; 6] = [65, 144, 66, 149, 147, 150];

    #[test]
    fn textpos_test_is_the_brute_force_class() {
        let max = i32::MAX;
        let ss: Vec<i32> = (0..=5).chain(max - 5..=max).collect(); // alloc: Cold
        for op in INT4_CMP {
            for left in [true, false] {
                for k in [i32::MIN, -2, -1, 0, 1, 2, 3, max - 1, max] {
                    let at = |s: i32| {
                        let (a, b) = if left { (s, k) } else { (k, s) };
                        match op { 65 => a == b, 144 => a != b, 66 => a < b, 149 => a <= b, 147 => a > b, 150 => a >= b, _ => panic!("{op}") }
                    };
                    let v = at(1);
                    let want = (ss[1..].iter().all(|&s| at(s) == v) && at(0) != v).then_some(v);
                    assert_eq!(textpos_test(op, k, left), want, "op {op} k {k} left {left}");
                }
            }
        }
        let cases = [
            (147, 0, true, Some(true)), (144, 0, true, Some(true)), (150, 1, true, Some(true)), (66, 0, false, Some(true)),
            (65, 0, true, Some(false)), (66, 1, true, Some(false)), (149, 0, true, Some(false)), (147, 1, false, Some(false)),
            (147, 1, true, None), (65, 3, true, None), (147, -1, true, None), (150, 0, true, None), (66, max, true, None),
            (149, max, true, None), (67, 0, true, None),
        ];
        for (op, k, left, want) in cases {
            assert_eq!(textpos_test(op, k, left), want, "op {op} k {k} left {left}");
        }
    }

    #[test]
    fn each_spelling_is_its_twin() {
        assert_eq!(twin_spelling(true, 0, None, None), Some((false, true, false)), "starts_with: prefix");
        assert_eq!(twin_spelling(false, 147, Some(true), Some(0)), Some((true, true, false)), "strpos(x, c) > 0");
        assert_eq!(twin_spelling(false, 65, Some(true), Some(0)), Some((true, true, true)), "position(c IN x) = 0: NOT LIKE");
        assert_eq!(twin_spelling(false, 66, Some(false), Some(0)), Some((true, true, false)), "0 < strpos(x, c)");
        assert_eq!(twin_spelling(false, 147, Some(true), Some(5)), None, "strpos(x, c) > 5: a position test");
        assert_eq!(twin_spelling(false, 147, None, Some(0)), None, "not over a textpos call");
        assert_eq!(twin_spelling(false, 147, Some(true), None), None, "no constant");
        assert_eq!(twin_spelling(false, 868, None, None), None, "strpos as a value");
        assert_eq!(twin_pattern(b"g%_\\", true, true), b"%g\\%\\_\\\\%".to_vec(), "escaped, both ends");
        assert_eq!(twin_pattern(b"", false, true), b"%".to_vec(), "starts_with(x, '')");
    }
}
