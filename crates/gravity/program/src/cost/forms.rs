//! Four lowering form choices (PLANNER-CHANGES row R5-B) read from the class
//! table's measured rows (`cost_profile::measured`, written by scripts/costcal/emit-measured.py --r5 from r5.py's arms:
//! each row the difference of two forms over the same rows or a slope over a sweep), never a fitted constant.
//!
//! A lowering makes one program for every session (the process-wide program tier) and sees only the server's widest width
//! (`Facts::width`), so each choice is judged at both ends: the leader's rows (W0) and the rows at the widest measured width
//! at or under the server's (`measured::at`). A form is taken only where it wins at both ends; where the ends disagree the
//! caller keeps its rule of before, so no width runs slower than before. A reader answers `None` only for a class table
//! without its rows (`tests::every_reader_reads_its_rows` fails first); the caller then keeps its rule of before too.
use cost_profile::measured::at;

fn ns(primitive: &str, width: u16) -> Option<f64> {
    at(primitive, u32::from(width)).map(|r| r.ns).filter(|v| v.is_finite())
}

/// The ends a program is judged at: the leader alone and the server's widest width.
fn ends(widest: u16) -> [u16; 2] {
    [0, widest]
}

/// EXTREME_PRIM_MIN's rows by element class: (the extreme's price a row over the unrolled base, one compare's price an
/// element a row), for `< ANY` and `>= ALL`.
const EXTREME: [(&str, [(&str, &str); 2]); 3] = [
    ("int4", [("expr.saop.extreme.int4.lt_any", "expr.cmp.prim.int4.lt_any"), ("expr.saop.extreme.int4.ge_all", "expr.cmp.prim.int4.ge_all")]),
    ("int8", [("expr.saop.extreme.int8.lt_any", "expr.cmp.prim.int8.lt_any"), ("expr.saop.extreme.int8.ge_all", "expr.cmp.prim.int8.ge_all")]),
    ("float8", [("expr.saop.extreme.float8.lt_any", "expr.cmp.prim.float8.lt_any"), ("expr.saop.extreme.float8.ge_all", "expr.cmp.prim.float8.ge_all")]),
];

/// The measured class of a by-value list element: float4 / float8 read the float8 rows, another 8-byte type the int8
/// rows, the rest (2- and 4-byte integers, date, oid, bool) the int4 rows.
fn prim_class(typ: u32, len: i16) -> usize {
    match (typ, len) {
        (700 | 701, _) => 2,
        (_, 8) => 1,
        _ => 0,
    }
}

/// EXTREME_PRIM_MIN's reader (lowerv4 step/expr_saop.rs `extreme_of`): the shortest constant list under an ordering
/// operator gravity_expr compares in line that takes the list's extreme instead, the first count past the crossover (the
/// extreme's price a row over the unrolled base / one compare's) of both measured senses at both ends. On the R5 rows:
/// int4 and int8 15, float8 13 (the rule of before: `cost::expr::MAX_LIST` + 1).
pub fn extreme_prim_min(typ: u32, len: i16, widest: u16) -> Option<usize> {
    let (_, senses) = EXTREME[prim_class(typ, len)];
    let mut n = 0.0f64;
    for w in ends(widest) {
        for (extreme, compare) in senses {
            let (x, c) = (ns(extreme, w)?, ns(compare, w)?);
            if c <= 0.0 {
                return None;
            }
            n = n.max(x / c);
        }
    }
    Some(n.max(0.0).floor() as usize + 1)
}

/// TEXT_SET_MIN's rows: (the walk's price a row at no element, a compared element's price a row, the byte set's price a
/// row) of each measured case (a miss at the element's length, a miss at another length, half the rows hitting; elements of
/// 4, 32 and 1000 bytes).
const TEXT_SET: [(&str, &str, &str); 7] = [
    ("expr.saop.text_walk.base.hit_half.L4", "expr.saop.text_walk.hit_half.L4", "expr.saop.byteset.hit_half.L4"),
    ("expr.saop.text_walk.base.miss_eq.L4", "expr.saop.text_walk.miss_eq.L4", "expr.saop.byteset.miss_eq.L4"),
    ("expr.saop.text_walk.base.miss_eq.L32", "expr.saop.text_walk.miss_eq.L32", "expr.saop.byteset.miss_eq.L32"),
    ("expr.saop.text_walk.base.miss_eq.L1000", "expr.saop.text_walk.miss_eq.L1000", "expr.saop.byteset.miss_eq.L1000"),
    ("expr.saop.text_walk.base.miss_len.L4", "expr.saop.text_walk.miss_len.L4", "expr.saop.byteset.miss_len.L4"),
    ("expr.saop.text_walk.base.miss_len.L32", "expr.saop.text_walk.miss_len.L32", "expr.saop.byteset.miss_len.L32"),
    ("expr.saop.text_walk.base.miss_len.L1000", "expr.saop.text_walk.miss_len.L1000", "expr.saop.byteset.miss_len.L1000"),
];

/// TEXT_SET_MIN's reader (lowerv4 step/expr_saop.rs `bytes_list`): the shortest constant text list expr.saop answers
/// through its byte set, the first count from 2 (PostgreSQL plans a one-element IN as the equality) at which the set's
/// price a row is at most the walk's (base + one compare an element) in every measured case at the leader's rows and the
/// widest measured (a list's form is chosen without the relation's facts). On the R5 rows: 2, as before.
pub fn text_set_min() -> Option<usize> {
    let mut n = 2usize;
    for w in ends(u16::MAX) {
        for (base, elem, set) in TEXT_SET {
            let (b, e, s) = (ns(base, w)?, ns(elem, w)?, ns(set, w)?);
            if e <= 0.0 {
                return None;
            }
            n = n.max(((s - b) / e).max(0.0).ceil() as usize);
        }
    }
    Some(n)
}

/// PERCODE_DATUM_*'s rows by body class (the hosted md5 body's, then a bpchar range's): the datum's price a row, the
/// table's read a row, the table's price an entry.
const PERCODE: [(&str, &str, &str); 2] = [
    ("percode.datum.md5", "percode.table.read.md5", "percode.table.entry.md5"),
    ("percode.datum.cheap", "percode.table.read.cheap", "percode.table.entry.cheap"),
];

/// PERCODE_DATUM_*'s reader (lowerv4 dict/percode.rs `attach`): a per-code body over a column of `entries` entries in a
/// relation of `rows` rows reads the datum (`Some(true)`) or the per-code table (`Some(false)`) where that form's price
/// (the table: rows x a read + entries x an entry; the datum: rows x its price) is at most the other's at both ends; `None`
/// where the ends disagree (the caller keeps `PERCODE_DATUM_PCT` / `PERCODE_DATUM_ROWS_MIN`). `transform`: the body calls
/// a function gravity_expr does not open-code under another call (it transforms the column before it compares: the md5
/// rows), else the bpchar range's rows. No row minimum: the R5 row sweep (1K to 64K rows, an entry a row) reads the datum
/// cheaper at every count at both ends. On the R5 rows: a compare body reads the datum from 47.3 % entries a row, a
/// transforming one from 99.1 %, and keeps the table under 7.3 %.
pub fn percode_datum(entries: u64, rows: u64, transform: bool, widest: u16) -> Option<bool> {
    let (datum, read, entry) = PERCODE[usize::from(!transform)];
    let (mut datum_wins, mut table_wins) = (true, true);
    for w in ends(widest) {
        let table = rows as f64 * ns(read, w)? + entries as f64 * ns(entry, w)?;
        let datum = rows as f64 * ns(datum, w)?;
        datum_wins &= datum <= table;
        table_wins &= table <= datum;
    }
    match (datum_wins, table_wins) {
        (true, _) => Some(true),
        (false, true) => Some(false),
        (false, false) => None,
    }
}

/// VALUES_CODED_CELLS_MAX's rows: (the measured count of coded cells, the compiled list's first execution, the streamed
/// list's first execution), statement ns, W0.
const VALUES: [(usize, &str, &str); 8] = [
    (1, "values.coded.first.n1", "values.stream.first.n1"),
    (2, "values.coded.first.n2", "values.stream.first.n2"),
    (4, "values.coded.first.n4", "values.stream.first.n4"),
    (8, "values.coded.first.n8", "values.stream.first.n8"),
    (16, "values.coded.first.n16", "values.stream.first.n16"),
    (32, "values.coded.first.n32", "values.stream.first.n32"),
    (64, "values.coded.first.n64", "values.stream.first.n64"),
    (128, "values.coded.first.n128", "values.stream.first.n128"),
];

/// VALUES_CODED_CELLS_MAX's reader (lowerv4 ops/shared/values.rs `streams_past_cut`): the most cells that are not a Const
/// a VALUES list compiles (past it a list that can stream streams): the largest measured count whose compiled first
/// execution (the lowering and the tier-0 run) is at most the streamed one at it and at every measured count under it,
/// at W0 (a VALUES list never runs in parallel); 0 when the first measured count already streams cheaper. The decision is
/// the first execution's (a later one compiles on its repeat, and from the fourth the two forms tie). On the R5 rows: 1.
pub fn values_coded_cells_max() -> Option<usize> {
    let mut best = 0;
    for (n, coded, streamed) in VALUES {
        if ns(coded, 0)? > ns(streamed, 0)? {
            break;
        }
        best = n;
    }
    Some(best)
}

/// join.payload_fp's reader (lowerv4 ops/hashjoin/strategies/payload_fp.rs `pays`): a build row's store of a text key's
/// Fp128 words (its table entry and two words; costcal pfp's intercept) in permille of what a row reaching the group saves
/// (its render and SipHash against two loads; pfp's slope over the drained rows), the dearer of the two ends, rounded up:
/// the words pay where the rows at the group times 1000 reach the build's rows times it.
pub fn payload_fp_permille(widest: u16) -> Option<u64> {
    let mut r = 0.0f64;
    for w in ends(widest) {
        let (store, saved) = (ns("join.payload_fp.store", w)?, ns("join.payload_fp.saved", w)?);
        if saved <= 0.0 {
            return None;
        }
        r = r.max(store.max(0.0) / saved);
    }
    Some((r * 1000.0).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reader finds its rows in the class table (a missing row is this failure, never a default at run time).
    #[test]
    fn every_reader_reads_its_rows() {
        for (typ, len) in [(23, 4), (20, 8), (701, 8), (700, 4), (21, 2), (1114, 8)] {
            for widest in [0, 8, 16] {
                assert!(extreme_prim_min(typ, len, widest).is_some(), "extreme {typ} at {widest}");
            }
        }
        assert!(text_set_min().is_some());
        assert!(values_coded_cells_max().is_some());
        for transform in [false, true] {
            assert!(percode_datum(1, 1, transform, 16).is_some(), "an entry a row decides at both ends");
        }
        for widest in [0, 8, 16] {
            assert!(payload_fp_permille(widest).is_some(), "payload_fp at {widest}");
        }
    }

    /// The R5 rows' answers, as the PLANNER-CHANGES row R5-B states them.
    #[test]
    fn the_r5_rows_answers() {
        assert_eq!((extreme_prim_min(23, 4, 16), extreme_prim_min(20, 8, 16), extreme_prim_min(701, 8, 16)), (Some(15), Some(15), Some(13)));
        assert_eq!(extreme_prim_min(23, 4, 0), Some(15), "the leader alone: W0's rows at both ends");
        assert_eq!(text_set_min(), Some(2));
        assert_eq!(values_coded_cells_max(), Some(1));
        let pct = |n: u64, transform: bool| percode_datum(n, 1000, transform, 16);
        assert_eq!((pct(472, false), pct(474, false), pct(1000, false)), (None, Some(true), Some(true)), "a compare body: 47.3 %");
        assert_eq!((pct(990, true), pct(992, true), pct(1000, true)), (None, Some(true), Some(true)), "a transforming body: 99.1 %");
        assert_eq!((pct(72, true), pct(74, true)), (Some(false), None), "a transforming body keeps the table under 7.3 %");
        assert_eq!(percode_datum(4000, 4000, false, 16), Some(true), "no row minimum: an entry a row reads the datum");
        assert_eq!(percode_datum(400, 1000, false, 0), Some(false), "the leader alone: W0's rows decide");
        // costcal pfp (box 40, 10-15): a build row's store 4.06 ns against 61.90 ns saved a drained row at W0 (W8: no store)
        assert_eq!((payload_fp_permille(0), payload_fp_permille(16)), (Some(66), Some(66)), "the words pay from 6.6 % of the build's rows");
    }
}
