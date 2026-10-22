//! Monotone functions (D472, landing B: the FACTS monotone table, which PL-20's answer site and expr.monotone's MinMax
//! rewrite read): per audited built-in function and argument, the direction the result moves in with that argument
//! while every other argument is a constant, whether it is affine (slope ±1 inside its range), and whether it can raise.
//! Each row names its pg_proc proname (the unit test pins the oid to it) and is audited against its PostgreSQL 18 C body
//! (src/backend/utils/adt/int.c, int8.c); each is one of never_null.rs's audited functions (immutable, strict, never NULL
//! for non-NULL input). A row that raises does so only on a TAIL of the argument (integer overflow, a narrowing cast out
//! of range): the x on which f(x, c) raises lie below a bound, above one, or both, never between two x that do not.
//! Which tail depends on the constant's sign (x + c: the high one for c > 0, the low one for c < 0, none at 0; c - x:
//! the low one for c >= 0, none at -1, the high one for c <= -2), so a row records only that it can raise: the reader's
//! evaluation at both extremes, not this field, makes its answer exact.
//!
//! THE READER CONTRACT (PL-20 and expr.monotone's MinMax take it over word for word):
//! (a) MIN / MAX(f(x, c)) over the qualifying rows is f(MIN x) / f(MAX x), swapped for a decreasing f; and since the
//!     raising x form tails, PostgreSQL raises iff f raises at MIN x or at MAX x, so the reader evaluates f at both
//!     extremes whenever a row qualifies and raises if either does. No row, or only NULL x, gives NULL without f. A
//!     "constant" covers a run-time Param (a generic plan's $1), read once per execution; a NULL c gives NULL for every
//!     row with no call (f is strict), so nothing raises even where x + c would overflow.
//! (b) PostgreSQL reports the error of the first raising row in scan order. Folding f to the two extremes after the scan
//!     is PG-equal only when nothing else evaluated per row in that scan can raise (quals, FILTER clauses, the other
//!     aggregates' arguments and transitions, casts): otherwise a later row's division by zero would be reported where
//!     PG reports f's out of range. The reader takes the path only under that check, else keeps the per-row evaluation.
//! (c) DISTINCT and FILTER read the same extremes over the qualifying rows.
//! Floats, numeric, multiplication (its direction is the constant's sign) and text are not here: not monotone per
//! function, or not exact.

/// How a function's result moves with one argument, the others constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monotone {
    /// Non-decreasing in the argument (else non-increasing).
    pub increasing: bool,
    /// `±x + b` inside its range: the results' difference is the arguments' (a span carries over).
    pub affine: bool,
    /// Whether it can raise (only on a tail of the argument; which one depends on the constant, see above).
    pub raises: bool,
}

const UP: Monotone = Monotone { increasing: true, affine: true, raises: true };
const DOWN: Monotone = Monotone { increasing: false, affine: true, raises: true };
const WIDEN: Monotone = Monotone { increasing: true, affine: true, raises: false };

/// `(pg_proc oid, proname, argument, fact)`: each `+` is increasing in both arguments, each `-` in its first and
/// decreasing in its second, a unary minus decreasing (it raises at the type's minimum only).
const TABLE: &[(u32, &str, u8, Monotone)] = &[
    (176, "int2pl", 0, UP), (176, "int2pl", 1, UP),       // int.c:935
    (177, "int4pl", 0, UP), (177, "int4pl", 1, UP),       // int.c:820
    (178, "int24pl", 0, UP), (178, "int24pl", 1, UP),     // int.c:1017
    (179, "int42pl", 0, UP), (179, "int42pl", 1, UP),     // int.c:1078
    (180, "int2mi", 0, UP), (180, "int2mi", 1, DOWN),     // int.c:949
    (181, "int4mi", 0, UP), (181, "int4mi", 1, DOWN),     // int.c:834
    (182, "int24mi", 0, UP), (182, "int24mi", 1, DOWN),   // int.c:1031
    (183, "int42mi", 0, UP), (183, "int42mi", 1, DOWN),   // int.c:1092
    (212, "int4um", 0, DOWN),                             // int.c:800
    (213, "int2um", 0, DOWN),                             // int.c:915
    (462, "int8um", 0, DOWN),                             // int8.c:440
    (463, "int8pl", 0, UP), (463, "int8pl", 1, UP),       // int8.c:462
    (464, "int8mi", 0, UP), (464, "int8mi", 1, DOWN),     // int8.c:476
    (480, "int84", 0, UP),                                // int8.c:1249 (int8 -> int4: out of range raises, both tails)
    (481, "int48", 0, WIDEN),                             // int8.c:1241 (int4 -> int8)
    (837, "int82pl", 0, UP), (837, "int82pl", 1, UP),     // int8.c:1032
    (838, "int82mi", 0, UP), (838, "int82mi", 1, DOWN),   // int8.c:1046
    (841, "int28pl", 0, UP), (841, "int28pl", 1, UP),     // int8.c:1113
    (942, "int28mi", 0, UP), (942, "int28mi", 1, DOWN),   // int8.c:1127
    (1274, "int84pl", 0, UP), (1274, "int84pl", 1, UP),   // int8.c:890
    (1275, "int84mi", 0, UP), (1275, "int84mi", 1, DOWN), // int8.c:904
    (1278, "int48pl", 0, UP), (1278, "int48pl", 1, UP),   // int8.c:971
    (1279, "int48mi", 0, UP), (1279, "int48mi", 1, DOWN), // int8.c:985
];

/// Function `fnoid`'s monotone fact in argument `arg` (0-based), the others constant; None = not known monotone.
pub fn monotone(fnoid: u32, arg: usize) -> Option<Monotone> {
    TABLE.iter().find(|r| r.0 == fnoid && r.2 as usize == arg).map(|r| r.3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C body's arithmetic by proname (None = the body raises: the result is out of its type).
    fn eval(name: &str, a: &[i128]) -> Option<i128> {
        let (ws, _, r) = sig(name);
        assert!(a.iter().zip(&ws).all(|(&x, &b)| x >= lo(b) && x <= hi(b)), "{name}: an argument out of its type");
        let v = if name.ends_with("pl") {
            a[0] + a[1]
        } else if name.ends_with("mi") {
            a[0] - a[1]
        } else if name.ends_with("um") {
            -a[0]
        } else {
            a[0] // int84 / int48: the cast
        };
        (v >= lo(r) && v <= hi(r)).then_some(v)
    }

    /// `(argument widths, their count, result width)` in bytes, from the proname (intNM: N- and M-byte arguments).
    fn sig(name: &str) -> ([u8; 2], usize, u8) {
        let mut d = name.bytes().filter(u8::is_ascii_digit).map(|d| d - b'0');
        match (name, d.next(), d.next()) {
            ("int84", ..) => ([8, 0], 1, 4),
            ("int48", ..) => ([4, 0], 1, 8),
            (_, Some(w), None) if name.ends_with("um") => ([w, 0], 1, w),
            (_, Some(w), None) => ([w, w], 2, w),
            (_, Some(l), Some(r)) => ([l, r], 2, l.max(r)),
            _ => panic!("no signature for {name}"),
        }
    }

    fn lo(w: u8) -> i128 {
        -(1i128 << (8 * w - 1))
    }

    fn hi(w: u8) -> i128 {
        (1i128 << (8 * w - 1)) - 1
    }

    /// Points across width `w`'s domain, sorted: both bounds, their neighbours, small values, powers of two, and the
    /// off-by-one pairs around each bound the constant `c` moves (max - c and max - c + 1, min - c and min - c - 1, and
    /// c - max / c - min with theirs) for a result of width `r`.
    fn points(w: u8, c: i128, r: u8) -> Vec<i128> {
        let mut p = vec![lo(w), lo(w) + 1, lo(w) + 2, -2, -1, 0, 1, 2, hi(w) - 2, hi(w) - 1, hi(w)]; // alloc: Cold
        p.extend((1..8 * w as u32 - 1).flat_map(|k| [1i128 << k, -(1i128 << k)]));
        p.extend([hi(r) - c, hi(r) - c + 1, lo(r) - c, lo(r) - c - 1, c - hi(r), c - hi(r) - 1, c - lo(r), c - lo(r) + 1, hi(r), hi(r) + 1, lo(r), lo(r) - 1]);
        p.retain(|&x| x >= lo(w) && x <= hi(w));
        p.sort_unstable();
        p.dedup();
        p
    }

    /// Every row: its oid is its proname in pg_proc (crates/gravity/fn/function_tiers.tsv, generated from pg_proc.dat); an
    /// audited never-NULL function; for every constant of the other argument, over x's points in order, the
    /// non-raising results move in the row's direction with slope ±1 when affine, the raising x form a prefix and / or
    /// a suffix (tails), and a row that never raises never does.
    #[test]
    fn every_row_is_its_proname_monotone_affine_and_raises_on_tails_only() {
        let tiers = include_str!("../../../fn/function_tiers.tsv");
        for &(f, name, arg, m) in TABLE {
            let row = tiers.lines().find(|l| l.split('\t').next().and_then(|o| o.parse::<u32>().ok()) == Some(f));
            assert_eq!(row.and_then(|l| l.split('\t').nth(1)), Some(name), "oid {f}: not {name} in pg_proc");
            assert!(crate::catalog::never_null::audited(f), "{name}: not in never_null's audited set");
            let (ws, n, r) = sig(name);
            let unary = n == 1;
            let consts = if unary { vec![0] } else { points(ws[1 - arg as usize], 0, r) }; // alloc: Cold
            for c in consts {
                let xs = points(ws[arg as usize], if unary { 0 } else { c }, r);
                let out: Vec<Option<i128>> = xs.iter().map(|&x| eval(name, &(if arg == 0 { [x, c] } else { [c, x] })[..n])).collect();
                let ok: Vec<usize> = (0..xs.len()).filter(|&i| out[i].is_some()).collect();
                if let (Some(&a), Some(&b)) = (ok.first(), ok.last()) {
                    assert_eq!(b - a + 1, ok.len(), "{name} arg {arg} c {c}: a raising x between two that do not");
                }
                assert!(m.raises || ok.len() == xs.len(), "{name} arg {arg} c {c}: raises though the row says never");
                for w in ok.windows(2) {
                    let (dx, dy) = (xs[w[1]] - xs[w[0]], out[w[1]].unwrap() - out[w[0]].unwrap());
                    assert!(if m.increasing { dy >= 0 } else { dy <= 0 }, "{name} arg {arg} c {c}: direction");
                    assert!(!m.affine || dy == if m.increasing { dx } else { -dx }, "{name} arg {arg} c {c}: slope");
                }
            }
        }
    }

    /// The off-by-one at a bound: int4pl(x, 5) is in range at x = max - 5 and raises at max - 4; int4mi(5, x) raises at
    /// x = min + 5 (5 - x = max + 1) and not at min + 6; int4mi(-1, x) never raises; int4um raises at min only; int84
    /// raises just past int4's max.
    #[test]
    fn the_bounds_fall_where_the_c_bodies_put_them() {
        let (mx, mn) = (hi(4), lo(4));
        assert_eq!(eval("int4pl", &[mx - 5, 5]), Some(mx));
        assert_eq!(eval("int4pl", &[mx - 4, 5]), None);
        assert_eq!(eval("int4mi", &[5, mn + 5]), None, "5 - (min + 5) = max + 1");
        assert_eq!(eval("int4mi", &[5, mn + 6]), Some(mx));
        assert_eq!(eval("int4mi", &[-1, mn]), Some(mx), "c = -1: never raises");
        assert_eq!(eval("int4um", &[mn]), None);
        assert_eq!(eval("int4um", &[mn + 1]), Some(mx));
        assert_eq!(eval("int84", &[mx + 1]), None);
        assert_eq!(eval("int84", &[mn]), Some(mn));
    }

    #[test]
    fn an_unknown_function_or_argument_is_not_monotone() {
        assert_eq!(monotone(141, 0), None, "int4mul: its direction is the constant's sign");
        assert_eq!(monotone(212, 1), None, "int4um has one argument");
        assert_eq!(monotone(181, 1), Some(DOWN), "int4mi falls with its subtrahend");
    }
}
