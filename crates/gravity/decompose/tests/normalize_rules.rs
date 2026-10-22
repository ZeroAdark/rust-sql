//! `commute`, `retype`, `like_twin`, `regexp3` and their near forms. The witnesses against PG 18.6 (fallback off) are
//! the v5probe corpora under the lane's `witness/` directory (same answers).
use datum::Datum;
use mcx::{Mcx, MemoryContext};
use types_nodes::list::NodeList;
use types_nodes::primnodes::OpExpr;
use types_nodes::Node;

use gravity_decompose::normalize::{expr, is_canonical};
use gravity_decompose::catalog::NoFacts;

fn op<'p>(m: Mcx<'p>, opno: u32, opfuncid: u32, collid: u32, l: Node<'p>, r: Node<'p>) -> Node<'p> {
    let args = NodeList::from_slice(m, &[l, r]).unwrap();
    Node::mk(m, OpExpr { opno, opfuncid, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: collid, args, location: -1 }).unwrap()
}

fn int4<'p>(m: Mcx<'p>, null: bool) -> Node<'p> {
    Node::mk_const(m, types_core::INT4OID, -1, 0, 4, Datum::from_i32(5), null, true).unwrap()
}

fn var<'p>(m: Mcx<'p>, ty: u32, coll: u32) -> Node<'p> {
    Node::mk_var(m, 1, 1, ty, -1, coll, 0).unwrap()
}

/// `(opno, opfuncid, inputcollid, left is the given node)` of a rewritten binary operator.
fn shape(n: Node<'_>) -> (u32, u32, u32, Node<'_>) {
    let o = n.as_op_expr().unwrap();
    (o.opno, o.opfuncid, o.inputcollid, o.args.nth(0))
}

#[test]
fn a_constant_on_the_left_moves_right_under_the_commutator() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    // 5 < x (int4lt 97, int4lt 66) -> x > 5 (int4gt 521, int4gt 147)
    let (k, x) = (int4(m, false), var(m, types_core::INT4OID, 0));
    let before = op(m, 97, 66, 0, k, x);
    assert!(!is_canonical(&NoFacts, before));
    let after = expr(m, &NoFacts, before).unwrap();
    let (opno, func, _, left) = shape(after);
    assert_eq!((opno, func), (521, 147));
    assert!(left.ptr_eq(x) && after.as_op_expr().unwrap().args.nth(1).ptr_eq(k), "the same argument nodes, swapped");
    assert!(is_canonical(&NoFacts, after));
    // PG's node is left alone
    assert_eq!(before.as_op_expr().unwrap().opno, 97);
}

#[test]
fn near_forms_null_collation_cross_type_and_no_commutator() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    // NULL < x: still commutes (NULL either way)
    let after = expr(m, &NoFacts, op(m, 97, 66, 0, int4(m, true), var(m, types_core::INT4OID, 0))).unwrap();
    assert_eq!(shape(after).0, 521);
    // '' <> x COLLATE "C" (textne 531, self-commutator): the input collation stays
    let t = Node::mk_const(m, types_core::TEXTOID, -1, 950, -1, Datum::from_i64(0), true, false).unwrap();
    let after = expr(m, &NoFacts, op(m, 531, 157, 950, t, var(m, types_core::TEXTOID, 950))).unwrap();
    assert_eq!((shape(after).0, shape(after).1, shape(after).2), (531, 157, 950));
    // 5 < x::int8 (int48lt 37) -> x > 5 (int84gt 419), then retype -> x > 5::int8 (int8gt 413, int8gt 470)
    let after = expr(m, &NoFacts, op(m, 37, 854, 0, int4(m, false), var(m, types_core::INT8OID, 0))).unwrap();
    assert_eq!((shape(after).0, shape(after).1), (413, 470));
    // two constants, a column on the left, an operator outside the builtin catalog: unchanged
    for n in [op(m, 97, 66, 0, int4(m, false), int4(m, false)), op(m, 97, 66, 0, var(m, types_core::INT4OID, 0), int4(m, false)), op(m, 70000, 1, 0, int4(m, false), var(m, types_core::INT4OID, 0))] {
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    }
}

#[test]
fn nested_operators_rewrite_bottom_up() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    // 5 < x AND 5 < x: both arms of the BoolExpr rewrite
    let a = op(m, 97, 66, 0, int4(m, false), var(m, types_core::INT4OID, 0));
    let b = op(m, 97, 66, 0, int4(m, false), var(m, types_core::INT4OID, 0));
    let args = NodeList::from_slice(m, &[a, b]).unwrap();
    let and = Node::mk(m, types_nodes::primnodes::BoolExpr { boolop: types_nodes::primnodes::BoolExprType::AND_EXPR, args, location: -1 }).unwrap();
    let after = expr(m, &NoFacts, and).unwrap();
    assert!(is_canonical(&NoFacts, after));
    assert!(after.as_bool_expr().unwrap().args.iter().all(|x| x.as_op_expr().unwrap().opno == 521));
}

fn konst<'p>(m: Mcx<'p>, ty: u32, len: i32, v: Datum, null: bool) -> Node<'p> {
    Node::mk_const(m, ty, -1, 0, len, v, null, true).unwrap()
}

/// `(opno, opfuncid, the constant's type, value, null)` of a retyped comparison.
fn retyped(n: Node<'_>) -> (u32, u32, u32, i64, bool) {
    let o = n.as_op_expr().unwrap();
    let k = o.args.nth(1).as_const().unwrap();
    let v = if k.constlen == 8 { k.constvalue.as_i64() } else { k.constvalue.as_i32() as i64 };
    (o.opno, o.opfuncid, k.consttype, if k.constisnull { 0 } else { v }, k.constisnull)
}

const DAY: i64 = 86_400_000_000;
/// 2020-03-01 as a date (days since 2000-01-01).
const MAR1: i64 = 7365;

#[test]
fn a_cross_type_constant_takes_the_column_type() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x4 = var(m, types_core::INT4OID, 0);
    // x < 5::int8 (int48lt 37) -> x < 5 (int4lt 97, int4lt 66)
    let n = op(m, 37, 854, 0, x4, konst(m, types_core::INT8OID, 8, Datum::from_i64(5), false));
    assert!(!is_canonical(&NoFacts, n));
    let after = expr(m, &NoFacts, n).unwrap();
    assert_eq!(retyped(after), (97, 66, types_core::INT4OID, 5, false));
    assert!(after.as_op_expr().unwrap().args.nth(0).ptr_eq(x4) && is_canonical(&NoFacts, after));
    // 5::int8 > x (int84gt 419): commute (x < 5::int8), then retype
    let after = expr(m, &NoFacts, op(m, 419, 477, 0, konst(m, types_core::INT8OID, 8, Datum::from_i64(5), false), x4)).unwrap();
    assert_eq!(retyped(after), (97, 66, types_core::INT4OID, 5, false));
    // NULL::int8: a NULL of the column type
    let after = expr(m, &NoFacts, op(m, 37, 854, 0, x4, konst(m, types_core::INT8OID, 8, Datum::from_i64(0), true))).unwrap();
    assert_eq!(retyped(after), (97, 66, types_core::INT4OID, 0, true));
    // smallint x < 7 (int24lt 534) -> int2lt 95
    let after = expr(m, &NoFacts, op(m, 534, 160, 0, var(m, types_core::INT2OID, 0), int4(m, false))).unwrap();
    assert_eq!(retyped(after), (95, 64, types_core::INT2OID, 5, false));
}

#[test]
fn near_forms_overflow_parameter_and_same_type_stay() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x4 = var(m, types_core::INT4OID, 0);
    let big = konst(m, types_core::INT8OID, 8, Datum::from_i64(i32::MAX as i64 + 1), false);
    let small = konst(m, types_core::INT8OID, 8, Datum::from_i64(i32::MIN as i64 - 1), false);
    let param = Node::mk(m, types_nodes::primnodes::Param { paramtype: types_core::INT8OID, paramid: 1, paramtypmod: -1, ..Default::default() }).unwrap();
    for n in [op(m, 37, 854, 0, x4, big), op(m, 37, 854, 0, x4, small), op(m, 37, 854, 0, x4, param), op(m, 97, 66, 0, x4, int4(m, false))] {
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
        assert!(is_canonical(&NoFacts, n));
    }
    // the edge of the range converts
    let edge = konst(m, types_core::INT8OID, 8, Datum::from_i64(i32::MAX as i64), false);
    assert_eq!(retyped(expr(m, &NoFacts, op(m, 37, 854, 0, x4, edge)).unwrap()).3, i32::MAX as i64);
}

#[test]
fn a_timestamp_against_a_date_rounds_by_the_operator() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let d = var(m, types_core::DATEOID, 0);
    let ts = |t: i64| konst(m, types_core::TIMESTAMPOID, 8, Datum::from_i64(t), false);
    let noon = MAR1 * DAY + DAY / 2;
    // (cross-type operator, its function) -> (same-type operator, function, the date it compares with)
    let cases = [
        ((2345, 2338), noon, (1095, 1087, MAR1 + 1)), // d < noon     -> d < Mar 2
        ((2346, 2339), noon, (1096, 1088, MAR1)),     // d <= noon    -> d <= Mar 1
        ((2348, 2342), noon, (1098, 1090, MAR1 + 1)), // d >= noon    -> d >= Mar 2
        ((2349, 2341), noon, (1097, 1089, MAR1)),     // d > noon     -> d > Mar 1
        ((2347, 2340), MAR1 * DAY, (1093, 1086, MAR1)), // d = midnight -> d = Mar 1
        ((2345, 2338), MAR1 * DAY, (1095, 1087, MAR1)), // d < midnight -> d < Mar 1
        ((2345, 2338), -DAY / 2, (1095, 1087, 0)),      // before 2000-01-01: floor is toward minus infinity
        ((2349, 2341), i64::MAX, (1097, 1089, i32::MAX as i64)), // infinity -> infinity
        ((2345, 2338), i64::MIN, (1095, 1087, i32::MIN as i64)), // -infinity -> -infinity
    ];
    for ((op_, f), t, (want, wf, day)) in cases {
        let after = expr(m, &NoFacts, op(m, op_, f, 0, d, ts(t))).unwrap();
        assert_eq!(retyped(after), (want, wf, types_core::DATEOID, day, false), "operator {op_} at {t}");
    }
    // d = noon is false or NULL, never a date: it stays cross-type
    let n = op(m, 2347, 2340, 0, d, ts(noon));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    // timestamp x < date '2020-03-01' (2371) -> x < timestamp midnight (timestamp_lt 2062)
    let x = var(m, types_core::TIMESTAMPOID, 0);
    let after = expr(m, &NoFacts, op(m, 2371, 2364, 0, x, konst(m, types_core::DATEOID, 4, Datum::from_i32(MAR1 as i32), false))).unwrap();
    assert_eq!(retyped(after), (2062, 2054, types_core::TIMESTAMPOID, MAR1 * DAY, false));
}

const HOUR: i64 = 3_600_000_000;

/// A zone whose offset moves every day (east 3 h on even dates, west 2 h on odd ones), so the midnights are not a day
/// apart and the cast date of an instant may sit either side of the midnights that bracket it.
fn off(d: i64) -> i64 {
    if d.rem_euclid(2) == 0 { 3 * HOUR } else { -2 * HOUR }
}

fn mid(d: i64) -> i64 {
    let d = if d == SKIP { SKIP + 1 } else { d };
    d * DAY - off(d)
}

/// A local day the wobbly zone skips whole (as Pacific/Apia did at its date-line move): date2timestamptz reads its
/// midnight at the offset before the gap, so it is the next date's midnight (D3537).
const SKIP: i64 = MAR1 + 40;

/// The local date of `t` (the midnights bracket it), one later at AHEAD (a fall-back zone's repeated hour after midnight).
fn local(t: i64) -> i64 {
    let mut d = t.div_euclid(DAY);
    while mid(d) > t {
        d -= 1;
    }
    while mid(d + 1) <= t {
        d += 1;
    }
    d + i64::from(t == AHEAD)
}

const AHEAD: i64 = MAR1 * DAY + 5 * HOUR;

/// The wobbly zone's identity (`Zone::stamp`): a test moves it as `SET TimeZone` swaps the session's `pg_tz`.
static STAMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Whether the admission may convert (`Zone::active`): a test clears it as a named cursor's admission does.
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Held by every test that lends the wobbly zone, for its whole body: the zone is one per process (`set_zone`) and a test
/// moves `STAMP` / `ACTIVE`, so two of them on parallel test threads would read each other's moves (D3102).
static ZONE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[must_use = "hold the guard for the test's whole body"]
fn wobbly() -> std::sync::MutexGuard<'static, ()> {
    let held = ZONE_TESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // a test that failed while it held a move leaves it behind: every holder starts from the zone as lent
    STAMP.store(1, std::sync::atomic::Ordering::Relaxed);
    ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
    gravity_decompose::normalize::set_zone(gravity_decompose::normalize::Zone {
        date_tstz: |d| Some(mid(i64::from(d))),
        ts_tstz: |t| Some(t - off(t.div_euclid(DAY))),
        tstz_date: |t| i32::try_from(local(t)).ok(),
        spread: |_| Some((-2 * HOUR, 3 * HOUR)),
        stamp: || STAMP.load(std::sync::atomic::Ordering::Relaxed),
        active: || ACTIVE.load(std::sync::atomic::Ordering::Relaxed),
    });
    held
}

/// B2-31 / STEP-13: timestamptz against a date or timestamp takes the lent zone's casts; the class never reads the zone.
#[test]
fn a_timestamptz_compare_takes_the_session_zone() {
    let _zone = wobbly();
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let d = var(m, types_core::DATEOID, 0);
    let tz = |t: i64| konst(m, types_core::TIMESTAMPTZOID, 8, Datum::from_i64(t), false);
    let (lt, le, eq, ge, gt) = ((2358, 2351), (2359, 2352), (2360, 2353), (2361, 2355), (2362, 2354));
    let (dlt, dle, dge, dgt) = ((1095, 1087), (1096, 1088), (1098, 1090), (1097, 1089));
    let at = mid(MAR1); // Mar 1 is odd: its midnight is 02:00 UTC
    let cases = [
        (lt, at - HOUR, dlt, MAR1),          // d < 01:00Z    -> d < Mar 1 (Mar 1's midnight is later)
        (ge, at - HOUR, dge, MAR1),
        (le, at - HOUR, dle, MAR1 - 1),
        (gt, at - HOUR, dgt, MAR1 - 1),
        (lt, at, dlt, MAR1),                 // exactly Mar 1's midnight
        (le, at, dle, MAR1),
        (gt, at, dgt, MAR1),
        (ge, at + HOUR, dge, MAR1 + 1),      // past it: the next date's midnight
        (le, at + HOUR, dle, MAR1),
        (ge, AHEAD, dge, MAR1 + 1),          // a cast date one ahead of the bracket: the same bounds
        (le, AHEAD, dle, MAR1),
        (ge, mid(MAR1 + 1) + 1, dge, MAR1 + 2),
        (lt, mid(SKIP + 1), dlt, SKIP),      // the skipped date's midnight is the next one's: `<` / `>=` the skipped date,
        (ge, mid(SKIP + 1), dge, SKIP),
        (le, mid(SKIP + 1), dle, SKIP + 1),  // `<=` / `>` past both (D3537)
        (gt, mid(SKIP + 1), dgt, SKIP + 1),
        (lt, i64::MAX, dlt, i32::MAX as i64), // infinities -> the date infinities
        (ge, i64::MIN, dge, i32::MIN as i64),
    ];
    for ((op_, f), t, (want, wf), day) in cases {
        let n = op(m, op_, f, 0, d, tz(t));
        assert_eq!(gravity_decompose::normalize::retype_class(n), Some(true), "operator {op_} at {t}");
        assert_eq!(retyped(expr(m, &NoFacts, n).unwrap()), (want, wf, types_core::DATEOID, day, false), "operator {op_} at {t}");
    }
    // `=` against a date column stays cross-type at every instant (whether one is a midnight is the zone's), as does a
    // constant within ZONE_MARGIN of the range's end
    for n in [op(m, eq.0, eq.1, 0, d, tz(at)), op(m, eq.0, eq.1, 0, d, tz(at + HOUR)), op(m, ge.0, ge.1, 0, d, tz(9_223_371_331_200_000_000 - DAY))] {
        assert_eq!(gravity_decompose::normalize::retype_class(n), Some(false));
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    }
    // a timestamptz column: the date or timestamp constant cast as PG casts it, every operator
    let x = var(m, types_core::TIMESTAMPTZOID, 0);
    let date = konst(m, types_core::DATEOID, 4, Datum::from_i32(MAR1 as i32), false);
    let after = expr(m, &NoFacts, op(m, 2386, 2379, 0, x, date)).unwrap();
    assert_eq!(retyped(after), (1320, 1152, types_core::TIMESTAMPTZOID, at, false));
    let after = expr(m, &NoFacts, op(m, 2384, 2377, 0, x, date)).unwrap();
    assert_eq!(retyped(after), (1322, 1154, types_core::TIMESTAMPTZOID, at, false));
    // the last finite date has no timestamptz: kept (its day count times a day's microseconds overflows i64)
    let n = op(m, 2387, 2381, 0, x, konst(m, types_core::DATEOID, 4, Datum::from_i32(2_147_483_494), false));
    assert_eq!(gravity_decompose::normalize::retype_class(n), Some(false));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    let noon = MAR1 * DAY + 12 * HOUR;
    let after = expr(m, &NoFacts, op(m, 2543, 2531, 0, x, konst(m, types_core::TIMESTAMPOID, 8, Datum::from_i64(noon), false))).unwrap();
    assert_eq!(retyped(after), (1325, 1156, types_core::TIMESTAMPTZOID, noon + 2 * HOUR, false));
    // a timestamp column against timestamptz stays: a DST gap's wall times cast out of order
    let ts = var(m, types_core::TIMESTAMPOID, 0);
    let n = op(m, 2534, 2520, 0, ts, tz(noon));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    assert_eq!(gravity_decompose::normalize::retype_class(n), Some(false));
    // an admission's zone: a conversion since its first read holds the zone it was made in, which a later SET TimeZone
    // moves (exec's memo then admits again); an admission with no conversion holds none
    use gravity_decompose::normalize::{retype_value, zone_held, zone_moved, zone_reads};
    let since = zone_reads();
    assert_eq!(retype_value(types_core::DATEOID, types_core::TIMESTAMPTZOID, 4, Datum::from_i32(MAR1 as i32)), Some(Datum::from_i64(mid(MAR1))));
    let held = zone_held(since);
    assert_eq!(held, Some(1));
    assert!(!zone_moved(held) && !zone_moved(None));
    STAMP.store(2, std::sync::atomic::Ordering::Relaxed);
    assert!(zone_moved(held) && !zone_moved(None) && !zone_moved(zone_held(since)));
    STAMP.store(1, std::sync::atomic::Ordering::Relaxed);
    assert!(!zone_moved(held));
    // an admission that may not convert (a named cursor's) keeps every timestamptz compare as PG planned it
    let n = op(m, ge.0, ge.1, 0, d, tz(at + HOUR));
    assert_eq!(gravity_decompose::normalize::retype_class(n), Some(true));
    ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(gravity_decompose::normalize::retype_class(n), Some(false));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    assert_eq!(retype_value(types_core::DATEOID, types_core::TIMESTAMPTZOID, 4, Datum::from_i32(MAR1 as i32)), None);
    ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// D3740: a conversion on another thread (another session's admission) moves no count an admission on this thread reads,
/// so it never marks this admission as holding a constant of the session zone; this thread's own conversion does.
#[test]
fn another_threads_conversion_marks_no_admission_here() {
    let _zone = wobbly();
    use gravity_decompose::normalize::{retype_value, zone_held, zone_reads};
    let since = zone_reads();
    std::thread::spawn(|| {
        let at = zone_reads();
        assert!(retype_value(types_core::DATEOID, types_core::TIMESTAMPTZOID, 4, Datum::from_i32(MAR1 as i32)).is_some());
        assert_eq!(zone_held(at), Some(1));
    })
    .join()
    .unwrap();
    assert_eq!(zone_held(since), None);
    assert!(retype_value(types_core::DATEOID, types_core::TIMESTAMPTZOID, 4, Datum::from_i32(MAR1 as i32)).is_some());
    assert_eq!(zone_held(since), Some(1));
}

/// A UTF-8 database without a catalog: `like_twin` fires.
struct Utf8;

impl gravity_decompose::catalog::Facts for Utf8 {
    fn rel(&self, _: u32) -> Option<gravity_decompose::catalog::RelFact> {
        None
    }
    fn att(&self, _: u32, _: i16) -> Option<gravity_decompose::catalog::AttFact> {
        None
    }
    fn index(&self, _: u32) -> Option<gravity_decompose::catalog::IndexFact> {
        None
    }
    fn indexes_of(&self, _: u32, _: &mut dyn FnMut(gravity_decompose::catalog::IndexFact)) {}
    fn index_expr(&self, _: u32, _: u8) -> Option<gravity_decompose::catalog::ExprFp> {
        None
    }
    fn database_utf8(&self) -> bool {
        true
    }
}

const DEFAULT_COLL: u32 = 100;

fn text<'p>(m: Mcx<'p>, s: &str) -> Node<'p> {
    gravity_decompose::normalize::text_const(m, s.as_bytes(), DEFAULT_COLL).unwrap()
}

fn func<'p>(m: Mcx<'p>, funcid: u32, ret: u32, coll: u32, l: Node<'p>, r: Node<'p>) -> Node<'p> {
    let args = NodeList::from_slice(m, &[l, r]).unwrap();
    Node::mk(m, types_nodes::primnodes::FuncExpr { funcid, funcresulttype: ret, inputcollid: coll, args, location: -1, ..Default::default() }).unwrap()
}

fn int4v<'p>(m: Mcx<'p>, v: i32) -> Node<'p> {
    Node::mk_const(m, types_core::INT4OID, -1, 0, 4, Datum::from_i32(v), false, true).unwrap()
}

/// `(LIKE or NOT LIKE operator, pattern)` of a twin; none when `n` is not one.
fn twin(n: Node<'_>) -> Option<(u32, String)> {
    let o = n.as_op_expr().filter(|o| matches!(o.opno, 1209 | 1210))?;
    let c = o.args.nth(1).as_const()?;
    Some((o.opno, String::from_utf8(gravity_decompose::expr::const_bytes(c)?.to_vec()).unwrap()))
}

#[test]
fn text_searches_become_their_like_twin() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = var(m, gravity_decompose::catalog::TEXTOID, DEFAULT_COLL);
    let strpos = |needle: &str| func(m, 868, types_core::INT4OID, DEFAULT_COLL, x, text(m, needle));
    let cases: [(Node<'_>, Option<(u32, &str)>); 14] = [
        // starts_with(x, 'a%b') -> x LIKE 'a\%b%'
        (func(m, 3696, 16, DEFAULT_COLL, x, text(m, "a%b")), Some((1209, "a\\%b%"))),
        // x ^@ 'a_' (operator 3877, function starts_with)
        (op(m, 3877, 3696, DEFAULT_COLL, x, text(m, "a_")), Some((1209, "a\\_%"))),
        // strpos(x, 'ab') > 0 / >= 1 / <> 0: occurs; = 0 / < 1: does not
        (op(m, 521, 147, 0, strpos("ab"), int4v(m, 0)), Some((1209, "%ab%"))),
        (op(m, 525, 150, 0, strpos("ab"), int4v(m, 1)), Some((1209, "%ab%"))),
        (op(m, 518, 144, 0, strpos("a\\b"), int4v(m, 0)), Some((1209, "%a\\\\b%"))),
        (op(m, 96, 65, 0, strpos("ab"), int4v(m, 0)), Some((1210, "%ab%"))),
        // position(c IN x) is the same function family
        (op(m, 97, 66, 0, func(m, 849, types_core::INT4OID, DEFAULT_COLL, x, text(m, "ab")), int4v(m, 1)), Some((1210, "%ab%"))),
        // a position test is not a membership test
        (op(m, 521, 147, 0, strpos("ab"), int4v(m, 1)), None),
        // left(x, 2) = 'ab' -> LIKE 'ab%'; right(x, 1) <> 'é' (one character, two bytes) -> NOT LIKE '%é'
        (op(m, 98, 67, DEFAULT_COLL, func(m, 3060, 25, DEFAULT_COLL, x, int4v(m, 2)), text(m, "ab")), Some((1209, "ab%"))),
        (op(m, 531, 157, DEFAULT_COLL, func(m, 3061, 25, DEFAULT_COLL, x, int4v(m, 1)), text(m, "é")), Some((1210, "%é"))),
        // left(x, 3) = 'ab' is never true for a 3-character prefix: not the twin
        (op(m, 98, 67, DEFAULT_COLL, func(m, 3060, 25, DEFAULT_COLL, x, int4v(m, 3)), text(m, "ab")), None),
        // x ~ 'abc' -> LIKE '%abc%'; x !~ 'a_c' -> NOT LIKE '%a\_c%'; x ~ 'a.c' stays a regex
        (op(m, 641, 1254, DEFAULT_COLL, x, text(m, "abc")), Some((1209, "%abc%"))),
        (op(m, 642, 1256, DEFAULT_COLL, x, text(m, "a_c")), Some((1210, "%a\\_c%"))),
        (op(m, 641, 1254, DEFAULT_COLL, x, text(m, "a.c")), None),
    ];
    for (i, (before, want)) in cases.into_iter().enumerate() {
        let after = expr(m, &Utf8, before).unwrap();
        assert_eq!(twin(after), want.map(|(o, p)| (o, p.to_string())), "case {i}"); // alloc: Cold
        assert_eq!(after.ptr_eq(before), want.is_none(), "case {i}");
        assert!(is_canonical(&Utf8, after), "case {i}");
    }
}

#[test]
fn the_like_twin_keeps_the_function_where_bytes_are_not_characters() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = var(m, gravity_decompose::catalog::TEXTOID, DEFAULT_COLL);
    // not UTF-8 (a source without the encoding)
    let n = func(m, 3696, 16, DEFAULT_COLL, x, text(m, "ab"));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    // no collation, or a nondeterministic (user) one
    for coll in [0, 16384] {
        let n = func(m, 3696, 16, coll, x, text(m, "ab"));
        assert!(expr(m, &Utf8, n).unwrap().ptr_eq(n), "collation {coll}");
    }
    // a NULL needle
    let null = Node::mk_const(m, gravity_decompose::catalog::TEXTOID, -1, DEFAULT_COLL, -1, Datum::from_usize(0), true, false).unwrap();
    let n = func(m, 3696, 16, DEFAULT_COLL, x, null);
    assert!(expr(m, &Utf8, n).unwrap().ptr_eq(n));
    // a non-constant needle
    let n = func(m, 3696, 16, DEFAULT_COLL, x, x);
    assert!(expr(m, &Utf8, n).unwrap().ptr_eq(n));
}

fn regexp<'p>(m: Mcx<'p>, args: &[Node<'p>]) -> Node<'p> {
    let funcid = if args.len() == 3 { 2284 } else { 2285 };
    let args = NodeList::from_slice(m, args).unwrap();
    Node::mk(m, types_nodes::primnodes::FuncExpr { funcid, funcresulttype: gravity_decompose::catalog::TEXTOID, inputcollid: DEFAULT_COLL, args, location: -1, ..Default::default() }).unwrap()
}

#[test]
fn regexp_replace_flags_that_change_nothing_become_the_three_argument_form() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = var(m, gravity_decompose::catalog::TEXTOID, DEFAULT_COLL);
    let (p, r) = (text(m, "^https?://([^/]+)/.*$"), text(m, "\\1"));
    for flags in ["", "g"] {
        let before = regexp(m, &[x, p, r, text(m, flags)]);
        assert!(!is_canonical(&NoFacts, before), "flags {flags:?}");
        let after = expr(m, &NoFacts, before).unwrap();
        let f = after.as_func_expr().unwrap();
        assert_eq!((f.funcid, f.args.len(), f.inputcollid), (2284, 3, DEFAULT_COLL), "flags {flags:?}");
        assert!(f.args.nth(0).ptr_eq(x) && f.args.nth(1).ptr_eq(p) && f.args.nth(2).ptr_eq(r));
        assert!(is_canonical(&NoFacts, after));
        assert!(gravity_decompose::normalize::regexp_replace3(before).is_some() && gravity_decompose::normalize::regexp_replace3(after).is_some());
    }
    // flags that change the answer, 'g' on a pattern that can match twice, and a non-constant or NULL flags stay
    let null = Node::mk_const(m, gravity_decompose::catalog::TEXTOID, -1, DEFAULT_COLL, -1, Datum::from_usize(0), true, false).unwrap();
    let kept = [
        regexp(m, &[x, p, r, text(m, "i")]),
        regexp(m, &[x, p, r, text(m, "gn")]),
        regexp(m, &[x, text(m, "a.c"), r, text(m, "g")]),
        regexp(m, &[x, text(m, "^a|b"), r, text(m, "g")]),
        regexp(m, &[x, x, r, text(m, "g")]),
        regexp(m, &[x, p, r, x]),
        regexp(m, &[x, p, r, null]),
    ];
    for (i, n) in kept.into_iter().enumerate() {
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n), "case {i}");
        assert!(gravity_decompose::normalize::regexp_replace3(n).is_none(), "case {i}");
    }
}

#[test]
fn count_star_needs_count_any_without_distinct_over_a_never_null_argument() {
    use gravity_decompose::normalize::count_star;
    assert_eq!(count_star(2147, false, true), Some(2803));
    assert_eq!(count_star(2147, true, true), None, "count(DISTINCT x) counts values, not rows");
    assert_eq!(count_star(2147, false, false), None, "a nullable argument (or one under an outer join)");
    assert_eq!(count_star(2803, false, true), None, "already count(*)");
    assert_eq!(count_star(2108, false, true), None, "sum(int4)");
}

fn arith<'p>(m: Mcx<'p>, opno: u32, opfuncid: u32, l: Node<'p>, r: Node<'p>) -> Node<'p> {
    let args = NodeList::from_slice(m, &[l, r]).unwrap();
    Node::mk(m, OpExpr { opno, opfuncid, opresulttype: types_core::INT4OID, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap()
}

#[test]
fn a_commutative_product_puts_columns_first_in_order() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let col = |no: i32, att: i16| Node::mk_var(m, no, att, types_core::INT4OID, -1, 0, 0).unwrap();
    // b * a (int4mul 514, int4mul 141) -> a * b, the same operator
    let (a, b) = (col(1, 1), col(1, 2));
    let after = expr(m, &NoFacts, arith(m, 514, 141, b, a)).unwrap();
    let (opno, func, _, left) = shape(after);
    assert_eq!((opno, func), (514, 141));
    assert!(left.ptr_eq(a) && is_canonical(&NoFacts, after));
    // (a + b) * c -> c * (a + b): a column before an expression; the inner sum is already ordered
    let sum = arith(m, 551, 177, col(1, 1), col(1, 2));
    let after = expr(m, &NoFacts, arith(m, 514, 141, sum, col(2, 1))).unwrap();
    assert!(shape(after).3.as_var().is_some_and(|v| v.varno == 2));
    // near forms unchanged: already ordered, a constant (commute's form), a non-commutative minus (int4mi 555),
    // a comparison (int4eq 96, boolean), two expressions
    let e1 = arith(m, 551, 177, col(1, 1), col(1, 2));
    let e2 = arith(m, 551, 177, col(1, 1), col(1, 3));
    for n in [arith(m, 514, 141, col(1, 1), col(1, 2)), arith(m, 514, 141, col(1, 2), int4(m, false)), arith(m, 555, 181, col(1, 2), col(1, 1)),
        op(m, 96, 65, 0, col(1, 2), col(1, 1)), arith(m, 514, 141, e1, e2)] {
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    }
}

/// A constant `text[]` of `elems` (short-header elements, as PG packs them); `nulls` marks a NULL bitmap present.
fn text_array<'p>(m: Mcx<'p>, elems: &[&str], nulls: bool) -> Node<'p> {
    let mut b: Vec<u8> = Vec::new();
    for w in [1i32, if nulls { 28 } else { 0 }, 25, elems.len() as i32, 1] {
        b.extend_from_slice(&w.to_ne_bytes());
    }
    if nulls {
        b.extend_from_slice(&[0xff, 0, 0, 0]);
    }
    for e in elems {
        b.push(((e.len() as u8 + 1) << 1) | 1);
        b.extend_from_slice(e.as_bytes());
    }
    let mut image = (((b.len() + 4) as u32) << 2).to_ne_bytes().to_vec();
    image.extend_from_slice(&b);
    image.resize(image.len().div_ceil(4) * 4, 0);
    let words: &'static [u32] = Box::leak(image.chunks(4).map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]])).collect());
    Node::mk_const(m, 1009, -1, DEFAULT_COLL, -1, Datum::from_usize(words.as_ptr() as usize), false, false).unwrap()
}

fn saop<'p>(m: Mcx<'p>, use_or: bool, opno: u32, opfuncid: u32, l: Node<'p>, r: Node<'p>) -> Node<'p> {
    let args = NodeList::from_slice(m, &[l, r]).unwrap();
    Node::mk(m, types_nodes::primnodes::ScalarArrayOpExpr { opno, opfuncid, useOr: use_or, inputcollid: DEFAULT_COLL, args, ..Default::default() }).unwrap()
}

#[test]
fn like_any_over_a_constant_array_becomes_its_arms() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = var(m, types_core::TEXTOID, DEFAULT_COLL);
    // x LIKE ANY ('{a%,%b_}') -> x LIKE 'a%' OR x LIKE '%b_'
    let before = saop(m, true, 1209, 850, x, text_array(m, &["a%", "%b_"], false));
    assert!(!is_canonical(&NoFacts, before));
    let after = expr(m, &NoFacts, before).unwrap();
    let b = after.as_bool_expr().expect("an OR");
    assert_eq!(b.boolop, types_nodes::primnodes::BoolExprType::OR_EXPR);
    let arms: Vec<_> = (0..b.args.len()).map(|i| twin(b.args.nth(i)).unwrap()).collect();
    assert_eq!(arms, vec![(1209, "a%".to_string()), (1209, "%b_".to_string())]);
    assert!(b.args.nth(0).as_op_expr().unwrap().args.nth(0).ptr_eq(x));
    assert!(is_canonical(&NoFacts, after));
    // x NOT LIKE ALL ('{a%,b%}') -> an AND of NOT LIKE; one element -> the single arm
    let after = expr(m, &NoFacts, saop(m, false, 1210, 851, x, text_array(m, &["a%", "b%"], false))).unwrap();
    assert_eq!(after.as_bool_expr().unwrap().boolop, types_nodes::primnodes::BoolExprType::AND_EXPR);
    let after = expr(m, &NoFacts, saop(m, true, 1209, 850, x, text_array(m, &["q"], false))).unwrap();
    assert_eq!(twin(after), Some((1209, "q".to_string())));
    // no length cap: nine elements -> nine arms
    let many = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    let after = expr(m, &NoFacts, saop(m, true, 1209, 850, x, text_array(m, &many, false))).unwrap();
    assert_eq!(after.as_bool_expr().unwrap().args.len(), 9);
    // PS4-11: x LIKE ALL ('{a%,%b}') -> an AND of LIKE; x NOT LIKE ANY ('{a%,%b}') -> an OR of NOT LIKE, in order
    let after = expr(m, &NoFacts, saop(m, false, 1209, 850, x, text_array(m, &["a%", "%b"], false))).unwrap();
    let b = after.as_bool_expr().expect("an AND");
    assert_eq!(b.boolop, types_nodes::primnodes::BoolExprType::AND_EXPR);
    assert_eq!((0..b.args.len()).map(|i| twin(b.args.nth(i)).unwrap()).collect::<Vec<_>>(), vec![(1209, "a%".to_string()), (1209, "%b".to_string())]);
    let after = expr(m, &NoFacts, saop(m, true, 1210, 851, x, text_array(m, &["a%", "%b"], false))).unwrap();
    let b = after.as_bool_expr().expect("an OR");
    assert_eq!(b.boolop, types_nodes::primnodes::BoolExprType::OR_EXPR);
    assert_eq!((0..b.args.len()).map(|i| twin(b.args.nth(i)).unwrap()).collect::<Vec<_>>(), vec![(1210, "a%".to_string()), (1210, "%b".to_string())]);
    let after = expr(m, &NoFacts, saop(m, false, 1209, 850, x, text_array(m, &["q"], false))).unwrap();
    assert_eq!(twin(after), Some((1209, "q".to_string())), "LIKE ALL of one element: the single arm");
    // near forms stay: a NULL bitmap (ANY and ALL), a non-column left side
    for n in [
        saop(m, true, 1209, 850, x, text_array(m, &["a", "b"], true)),
        saop(m, false, 1209, 850, x, text_array(m, &["a", "b"], true)),
        saop(m, true, 1209, 850, text(m, "k"), text_array(m, &["a"], false)),
    ] {
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(n));
    }
}

/// PS1-08: relation 7's columns as the relcache gives them: 1 int4 under a VALID not-null constraint, 2 int4 without one
/// (nullable, or NOT NULL ... NOT VALID: the facts carry the validated state, D2838), 3 text under a valid one.
struct NotNull;

impl gravity_decompose::catalog::Facts for NotNull {
    fn rel(&self, _: u32) -> Option<gravity_decompose::catalog::RelFact> {
        None
    }
    fn att(&self, relid: u32, attno: i16) -> Option<gravity_decompose::catalog::AttFact> {
        let (t, len, byval, notnull) = match (relid, attno) {
            (7, 1) => (types_core::INT4OID, 4, true, true),
            (7, 2) => (types_core::INT4OID, 4, true, false),
            (7, 3) => (types_core::TEXTOID, -1, false, true),
            _ => return None,
        };
        Some(gravity_decompose::catalog::AttFact::plain(t, -1, len, byval, notnull))
    }
    fn index(&self, _: u32) -> Option<gravity_decompose::catalog::IndexFact> {
        None
    }
    fn indexes_of(&self, _: u32, _: &mut dyn FnMut(gravity_decompose::catalog::IndexFact)) {}
    fn index_expr(&self, _: u32, _: u8) -> Option<gravity_decompose::catalog::ExprFp> {
        None
    }
}

fn coalesce<'p>(m: Mcx<'p>, ty: u32, coll: u32, args: &[Node<'p>]) -> Node<'p> {
    let args = NodeList::from_slice(m, args).unwrap();
    Node::mk(m, types_nodes::primnodes::CoalesceExpr { coalescetype: ty, coalescecollid: coll, args, location: -1 }).unwrap()
}

#[test]
fn coalesce_over_a_valid_not_null_scan_column_is_the_column() {
    use gravity_decompose::normalize::scan_expr;
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let col = |a: i16, ty: u32, coll: u32| Node::mk_var(m, 1, a, ty, -1, coll, 0).unwrap();
    let scan = Some((1, 7));
    // coalesce(x, 0) > 5 over relation 7's NOT NULL int4 column (scan range table index 1) -> x > 5 (int4gt 521)
    let x = col(1, types_core::INT4OID, 0);
    let after = scan_expr(m, &NotNull, scan, op(m, 521, 147, 0, coalesce(m, types_core::INT4OID, 0, &[x, int4(m, false)]), int4(m, false))).unwrap();
    assert!(after.as_op_expr().unwrap().args.nth(0).ptr_eq(x));
    // coalesce(t, '') over the NOT NULL text column of the same collation -> t
    let t = col(3, types_core::TEXTOID, DEFAULT_COLL);
    assert!(scan_expr(m, &NotNull, scan, coalesce(m, types_core::TEXTOID, DEFAULT_COLL, &[t, text(m, "")])).unwrap().ptr_eq(t));
    // near forms stay
    let nulled = Node::mk(m, types_nodes::primnodes::Var { varnullingrels: types_nodes::bitmapset::Bitmapset::make_singleton(m, 3).unwrap(), ..*x.as_var().unwrap() }).unwrap();
    let up = Node::mk(m, types_nodes::primnodes::Var { varlevelsup: 1, varnullingrels: types_nodes::bitmapset::Bitmapset::empty(), ..*x.as_var().unwrap() }).unwrap();
    let i8k = konst(m, types_core::INT8OID, 8, Datum::from_i64(0), false);
    for (why, n, s) in [
        ("no valid not-null constraint (nullable or NOT VALID)", coalesce(m, types_core::INT4OID, 0, &[col(2, types_core::INT4OID, 0), int4(m, false)]), scan),
        ("no scan (expr)", coalesce(m, types_core::INT4OID, 0, &[x, int4(m, false)]), None),
        ("another relation", coalesce(m, types_core::INT4OID, 0, &[x, int4(m, false)]), Some((1, 8))),
        ("another range table index", coalesce(m, types_core::INT4OID, 0, &[x, int4(m, false)]), Some((2, 7))),
        ("an outer join's nullingrels", coalesce(m, types_core::INT4OID, 0, &[nulled, int4(m, false)]), scan),
        ("an outer level's Var", coalesce(m, types_core::INT4OID, 0, &[up, int4(m, false)]), scan),
        ("another result type", coalesce(m, types_core::INT8OID, 0, &[x, i8k]), scan),
        ("another collation", coalesce(m, types_core::TEXTOID, 950, &[t, text(m, "")]), scan),
        ("the column not first", coalesce(m, types_core::INT4OID, 0, &[int4(m, false), x]), scan),
    ] {
        assert!(scan_expr(m, &NotNull, s, n).unwrap().ptr_eq(n), "{why}");
    }
}

#[test]
fn a_whole_filter_drops_a_coalesce_whose_constant_fails_it() {
    use gravity_decompose::normalize::{coalesce_filter_class, scan_filter};
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let scan = Some((1, 7));
    // relation 7's nullable int4 column 2: coalesce(x, c) op k
    let x = Node::mk_var(m, 1, 2, types_core::INT4OID, -1, 0, 0).unwrap();
    let cx = |c| coalesce(m, types_core::INT4OID, 0, &[x, c]);
    let k = |v: i32| int4v(m, v);
    let ge = |l, r| op(m, 525, 150, 0, l, r);
    let le = |l, r| op(m, 523, 149, 0, l, r);
    let fires = |n| {
        let after = scan_filter(m, &NotNull, scan, n).unwrap();
        let o = after.as_op_expr()?;
        o.args.nth(0).ptr_eq(x).then_some((o.opno, o.args.nth(1)))
    };
    // BETWEEN 1500000 AND 1501000 over coalesce(x, 0): 0 >= 1500000 is false, so the lower bound drops the COALESCE
    let lo = k(1_500_000);
    assert_eq!(fires(ge(cx(k(0)), lo)).map(|(o, r)| (o, r.ptr_eq(lo))), Some((525, true)));
    // 0 <= 1501000 is true: a NULL x passes it, so the upper bound keeps it
    let hi = le(cx(k(0)), k(1_501_000));
    assert!(scan_filter(m, &NotNull, scan, hi).unwrap().ptr_eq(hi));
    // the constant on the left: `commute`'s form is read (1500000 <= coalesce(x, 0) is coalesce(x, 0) >= 1500000)
    assert_eq!(fires(le(k(1_500_000), cx(k(0)))).map(|(o, _)| o), Some(525));
    // a NULL k: c op k is NULL
    assert!(fires(ge(cx(k(0)), int4(m, true))).is_some());
    // = and <>: 5 = 7 is false (rewritten), 7 = 7 true (kept); 5 <> 5 false (rewritten), 5 <> 6 true (kept)
    assert!(fires(op(m, 96, 65, 0, cx(k(5)), k(7))).is_some());
    assert!(fires(op(m, 96, 65, 0, cx(k(7)), k(7))).is_none());
    assert!(fires(op(m, 518, 144, 0, cx(k(5)), k(5))).is_some());
    assert!(fires(op(m, 518, 144, 0, cx(k(5)), k(6))).is_none());
    // the front key's class is the rule's decision, from the plan's form
    assert_eq!(coalesce_filter_class(ge(cx(k(0)), k(1_500_000))), Some(true));
    assert_eq!(coalesce_filter_class(hi), Some(false));
    assert_eq!(coalesce_filter_class(le(k(1_500_000), cx(k(0)))), Some(true));
    assert_eq!(coalesce_filter_class(ge(x, k(5))), None);
    // near forms stay whole
    let fail = ge(cx(k(0)), k(1_500_000));
    let not = Node::mk(m, types_nodes::primnodes::BoolExpr { boolop: types_nodes::primnodes::BoolExprType::NOT_EXPR, args: NodeList::from_slice(m, &[fail]).unwrap(), location: -1 }).unwrap();
    let or = Node::mk(m, types_nodes::primnodes::BoolExpr { boolop: types_nodes::primnodes::BoolExprType::OR_EXPR, args: NodeList::from_slice(m, &[fail, ge(x, k(9))]).unwrap(), location: -1 }).unwrap();
    let three = ge(coalesce(m, types_core::INT4OID, 0, &[x, Node::mk_var(m, 1, 1, types_core::INT4OID, -1, 0, 0).unwrap(), k(0)]), k(1_500_000));
    let column = ge(coalesce(m, types_core::INT4OID, 0, &[x, Node::mk_var(m, 1, 1, types_core::INT4OID, -1, 0, 0).unwrap()]), k(1_500_000));
    let t = Node::mk_var(m, 1, 4, types_core::TEXTOID, -1, DEFAULT_COLL, 0).unwrap();
    let textual = op(m, 98, 67, DEFAULT_COLL, coalesce(m, types_core::TEXTOID, DEFAULT_COLL, &[t, text(m, "")]), text(m, "a"));
    let i8k = konst(m, types_core::INT8OID, 8, Datum::from_i64(1_500_000), false);
    let cross = op(m, 76, 855, 0, cx(k(0)), i8k);
    for (why, n, s) in [("under NOT", not, scan), ("under OR", or, scan), ("no scan", fail, None), ("three arguments", three, scan), ("a column second", column, scan), ("text (no word)", textual, scan)] {
        assert!(scan_filter(m, &NotNull, s, n).unwrap().ptr_eq(n), "{why}");
    }
    // a constant `retype` rebuilt keeps the COALESCE (the rule reads the plan's constants only)
    let after = scan_filter(m, &NotNull, scan, cross).unwrap();
    assert!(after.as_op_expr().unwrap().args.nth(0).as_coalesce_expr().is_some(), "retyped");
}

/// SRC-07 rule `implied`: a timestamp column against a timestamptz constant keeps its cross-type compare and gains the
/// same-type bound it implies in the session zone (widened by the zone's offset spread), `=` both bounds; a date column
/// `= timestamptz K` gains `>=` / `<=` bounds holding exactly the dates whose midnight is K (two at a skipped day, D3537).
/// Every wall time the cross-type compare keeps satisfies the bound (checked over the wobbly zone, whose offset moves 5 h
/// a day, at quarter hours around several constants).
#[test]
fn a_timestamptz_compare_gains_its_implied_same_type_bound() {
    use gravity_decompose::normalize::{implied_class_of, retype_value, scan_filter, IMPLIED};
    let _zone = wobbly();
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let ts_tstz = |x: i64| x - off(x.div_euclid(DAY));
    let tz = |t: i64| konst(m, types_core::TIMESTAMPTZOID, 8, Datum::from_i64(t), false);
    let base = MAR1 * DAY;
    for k in [base - 7 * HOUR, base, base + HOUR, base + 5 * HOUR, mid(MAR1 + 1), mid(MAR1 + 2) + 1] {
        let bound = |s: u8| retype_value(types_core::TIMESTAMPTZOID, types_core::TIMESTAMPOID, s | IMPLIED, Datum::from_i64(k)).unwrap().as_i64();
        let (lt, le, ge, gt) = (bound(1), bound(2), bound(4), bound(5));
        for q in -4 * 24 * 4..4 * 24 * 4 {
            let x = k + q * HOUR / 4;
            let f = ts_tstz(x);
            assert!(f >= k || x < lt, "x < K at {x} vs {k}");
            assert!(f > k || x <= le, "x <= K at {x} vs {k}");
            assert!(f < k || x >= ge, "x >= K at {x} vs {k}");
            assert!(f <= k || x > gt, "x > K at {x} vs {k}");
        }
        // a date equal to K is one between its bounds: the least date whose midnight is >= K, the greatest whose is <= K
        let date = |s: u8| retype_value(types_core::TIMESTAMPTZOID, types_core::DATEOID, s | IMPLIED, Datum::from_i64(k)).unwrap().as_i32() as i64;
        let (lo, hi) = (date(4), date(2));
        for day in MAR1 - 3..MAR1 + 4 {
            assert_eq!(mid(day) == k, lo <= day && day <= hi, "date {day} = {k}");
        }
    }
    // D3537: a skipped local day shares the next date's midnight, so a date equal to that instant is either of them
    let k = mid(SKIP + 1);
    let date = |s: u8| retype_value(types_core::TIMESTAMPTZOID, types_core::DATEOID, s | IMPLIED, Datum::from_i64(k)).unwrap().as_i32() as i64;
    assert_eq!((date(4), date(2)), (SKIP, SKIP + 1));
    // the scan's leading filter: x < K (2534) keeps its qual and gains x < K' (timestamp_lt 2062) just before it; x = K (2536) gains
    // x >= (2065) and x <= (2063); a date d = K (2360) gains d >= (date_ge 1098) and d <= (date_le 1096); any other date
    // compare is retype's
    let x = var(m, types_core::TIMESTAMPOID, 0);
    let d = var(m, types_core::DATEOID, 0);
    let scan = Some((1, 16384));
    for ((opno, f), col, want) in [((2534, 2520), x, &[2062][..]), ((2536, 2522), x, &[2065, 2063]), ((2538, 2523), x, &[2064]), ((2360, 2353), d, &[1098, 1096])] {
        let n = op(m, opno, f, 0, col, tz(base + HOUR));
        assert_eq!(implied_class_of(n), Some(true));
        let list = Node::mk_list(m, NodeList::from_slice(m, &[n]).unwrap()).unwrap();
        let after = scan_filter(m, &NoFacts, scan, list).unwrap();
        let qs: Vec<Node<'_>> = after.as_list().expect("a qual list").iter().collect();
        assert_eq!(qs.len(), 1 + want.len(), "operator {opno}");
        assert_eq!(qs[want.len()].as_op_expr().unwrap().opno, opno, "the qual stays, cross-type, after its implied bounds");
        let got: Vec<u32> = qs[..want.len()].iter().map(|q| q.as_op_expr().unwrap().opno).collect();
        assert_eq!(got, want, "operator {opno}");
    }
    // no scan, a NULL constant or an infinity: nothing added (an inactive zone is `retype`'s test's: the flag is shared)
    let n = op(m, 2534, 2520, 0, x, tz(base));
    let list = Node::mk_list(m, NodeList::from_slice(m, &[n]).unwrap()).unwrap();
    assert!(scan_filter(m, &NoFacts, None, list).unwrap().ptr_eq(list));
    let inf = op(m, 2534, 2520, 0, x, tz(i64::MAX));
    assert_eq!(implied_class_of(inf), Some(false));
    let null = op(m, 2534, 2520, 0, x, konst(m, types_core::TIMESTAMPTZOID, 8, Datum::from_i64(0), true));
    assert_eq!(implied_class_of(null), Some(false));
}

fn case_test<'p>(m: Mcx<'p>, ty: u32) -> Node<'p> {
    Node::mk(m, types_nodes::primnodes::CaseTestExpr { typeId: ty, typeMod: -1, collation: 0 }).unwrap()
}

fn simple_case<'p>(m: Mcx<'p>, arg: Node<'p>, whens: &[(Node<'p>, Node<'p>)], default: Option<Node<'p>>) -> Node<'p> {
    use types_nodes::primnodes::{CaseExpr, CaseWhen};
    let arms: Vec<Node<'p>> = whens.iter().map(|&(t, r)| Node::mk(m, CaseWhen { expr: Some(t), result: Some(r), location: -1 }).unwrap()).collect();
    Node::mk(m, CaseExpr { casetype: types_core::INT4OID, casecollid: 0, arg: Some(arg), args: NodeList::from_slice(m, &arms).unwrap(), defresult: default, location: -1 }).unwrap()
}

/// FOLD-19 `case_simple`: `CASE x WHEN 5 THEN 1 WHEN 7 THEN 2 ELSE 0 END` is `CASE WHEN x = 5 THEN 1 WHEN x = 7 THEN 2 ELSE
/// 0 END` (each test with `x` in place of its CaseTestExpr; results, ELSE, type unchanged). Stays: a test value that acts
/// (random()), one holding a nested simple CASE in a test whose own test value acts, a by-reference test value (substr: the
/// searched form would evaluate it once per arm). Rewritten: word arithmetic of builtins (x % 3).
#[test]
fn a_simple_case_is_its_searched_form() {
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = var(m, types_core::INT4OID, 0);
    let k = |v: i32| int4v(m, v);
    let (r1, r2, d) = (k(1), k(2), k(0));
    let before = simple_case(m, x, &[(op(m, 96, 65, 0, case_test(m, types_core::INT4OID), k(5)), r1), (op(m, 96, 65, 0, case_test(m, types_core::INT4OID), k(7)), r2)], Some(d));
    assert!(!is_canonical(&NoFacts, before));
    let after = expr(m, &NoFacts, before).unwrap();
    let c = after.as_case_expr().expect("a CASE");
    assert!(c.arg.is_none() && c.defresult.is_some_and(|e| e.ptr_eq(d)) && c.casetype == types_core::INT4OID);
    let w: Vec<_> = c.args.iter().map(|w| w.as_case_when().unwrap()).collect();
    assert_eq!(w.len(), 2);
    assert!(w[0].result.unwrap().ptr_eq(r1) && w[1].result.unwrap().ptr_eq(r2));
    for t in &w {
        let o = t.expr.unwrap().as_op_expr().unwrap();
        assert_eq!((o.opno, o.opfuncid), (96, 65));
        assert!(o.args.nth(0).ptr_eq(x), "x in place of the CaseTestExpr");
    }
    assert!(is_canonical(&NoFacts, after));
    // PG's node is left alone
    assert!(before.as_case_expr().unwrap().arg.is_some());
    // a test value that acts (random(), VOLATILE): PG evaluates it once, so it stays
    let rnd = Node::mk(m, types_nodes::primnodes::FuncExpr { funcid: 1598, funcresulttype: types_core::FLOAT8OID, args: NodeList::nil(), ..Default::default() }).unwrap();
    let vol = simple_case(m, rnd, &[(op(m, 670, 293, 0, case_test(m, types_core::FLOAT8OID), k(1)), k(1))], None);
    assert!(expr(m, &NoFacts, vol).unwrap().ptr_eq(vol));
    assert!(is_canonical(&NoFacts, vol));
    // an outer test holding a nested simple CASE that stays (its test value acts): the outer stays too
    let inner = simple_case(m, rnd, &[(op(m, 670, 293, 0, case_test(m, types_core::FLOAT8OID), k(1)), k(5))], None);
    let outer = simple_case(m, x, &[(op(m, 96, 65, 0, case_test(m, types_core::INT4OID), inner), k(1))], None);
    assert!(expr(m, &NoFacts, outer).unwrap().as_case_expr().unwrap().arg.is_some());
    // a nested simple CASE whose test value does not act is rewritten first; then the outer one is
    let inner = simple_case(m, x, &[(op(m, 96, 65, 0, case_test(m, types_core::INT4OID), k(1)), k(5))], None);
    let outer = simple_case(m, x, &[(op(m, 96, 65, 0, case_test(m, types_core::INT4OID), inner), k(1))], None);
    let after = expr(m, &NoFacts, outer).unwrap();
    assert!(after.as_case_expr().unwrap().arg.is_none());
    assert!(is_canonical(&NoFacts, after));
    // word arithmetic of builtins (x % 3, int4mod) is cheap to evaluate in every test: rewritten
    let m3 = op(m, 530, 156, 0, x, k(3));
    let ar = simple_case(m, m3, &[(op(m, 96, 65, 0, case_test(m, types_core::INT4OID), k(0)), k(1)), (op(m, 96, 65, 0, case_test(m, types_core::INT4OID), k(1)), k(2))], None);
    assert!(expr(m, &NoFacts, ar).unwrap().as_case_expr().unwrap().arg.is_none(), "x % 3 is word arithmetic");
    // a by-reference test value (substr(t, 1, 4), text) would be evaluated in every test: PG's form stays (cheap_twice)
    let t = var(m, types_core::TEXTOID, 100);
    let sub = Node::mk(m, types_nodes::primnodes::FuncExpr { funcid: 936, funcresulttype: types_core::TEXTOID, args: NodeList::from_slice(m, &[t, k(1), k(4)]).unwrap(), ..Default::default() }).unwrap();
    let txt = simple_case(m, sub, &[(op(m, 98, 67, 100, case_test(m, types_core::TEXTOID), t), k(1))], None);
    assert!(expr(m, &NoFacts, txt).unwrap().ptr_eq(txt), "a text test value stays");
    assert!(is_canonical(&NoFacts, txt));
}

/// `x::text` over an integer `x` (CoerceViaIO, int8out / int4out / int2out).
fn as_text<'p>(m: Mcx<'p>, x: Node<'p>) -> Node<'p> {
    Node::mk(m, types_nodes::primnodes::CoerceViaIO { arg: x, resulttype: gravity_decompose::catalog::TEXTOID, resultcollid: DEFAULT_COLL, coerceformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap()
}

/// `(opno, opfuncid, the constant's type, value)` of a detexted comparison; none when it kept the text compare.
fn detexted(n: Node<'_>) -> Option<(u32, u32, u32, i64)> {
    let o = n.as_op_expr()?;
    o.args.nth(0).as_var()?;
    let k = o.args.nth(1).as_const()?;
    let v = match k.consttype {
        types_core::INT2OID => k.constvalue.as_i16() as i64,
        types_core::INT4OID => k.constvalue.as_i32() as i64,
        types_core::INT8OID => k.constvalue.as_i64(),
        _ => return None,
    };
    Some((o.opno, o.opfuncid, k.consttype, v))
}

#[test]
fn detext_compares_an_integer_to_the_canonical_text_of_a_value() {
    use gravity_decompose::normalize::{detext_class, detext_value};
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    // int8 = 410 (int8eq 467), <> 411 (int8ne 468); int4 = 96 (int4eq 65), <> 518 (int4ne 144); int2 = 94 (int2eq 63)
    let x8 = || var(m, types_core::INT8OID, 0);
    fn eq<'p>(m: Mcx<'p>, x: Node<'p>, s: &str) -> Node<'p> {
        op(m, 98, 67, DEFAULT_COLL, as_text(m, x), text(m, s))
    }
    let eq = |x, s| eq(m, x, s);
    assert_eq!(detexted(expr(m, &Utf8, eq(x8(), "1234567890123456789")).unwrap()), Some((410, 467, types_core::INT8OID, 1234567890123456789)));
    assert_eq!(detexted(expr(m, &Utf8, eq(x8(), "-9223372036854775808")).unwrap()), Some((410, 467, types_core::INT8OID, i64::MIN)));
    assert_eq!(detexted(expr(m, &Utf8, eq(x8(), "0")).unwrap()), Some((410, 467, types_core::INT8OID, 0)));
    assert_eq!(detexted(expr(m, &Utf8, op(m, 531, 157, DEFAULT_COLL, as_text(m, x8()), text(m, "7"))).unwrap()), Some((411, 468, types_core::INT8OID, 7)));
    assert_eq!(detexted(expr(m, &Utf8, eq(var(m, types_core::INT4OID, 0), "-42")).unwrap()), Some((96, 65, types_core::INT4OID, -42)));
    assert_eq!(detexted(expr(m, &Utf8, eq(var(m, types_core::INT2OID, 0), "300")).unwrap()), Some((94, 63, types_core::INT2OID, 300)));
    // the constant on the left: commute, then detext
    assert_eq!(detexted(expr(m, &Utf8, op(m, 98, 67, DEFAULT_COLL, text(m, "7"), as_text(m, x8()))).unwrap()), Some((410, 467, types_core::INT8OID, 7)));
    // no canonical output text: the text compare stays (PG answers false for every non-NULL x)
    for s in ["007", "+7", " 7", "7 ", "-0", "-", "", "9223372036854775808", "1e3", "٣"] {
        let n = eq(x8(), s);
        assert_eq!(detexted(expr(m, &Utf8, n).unwrap()), None, "{s:?}");
        assert_eq!(detext_class(n), Some(false), "{s:?}");
    }
    assert_eq!(detexted(expr(m, &Utf8, eq(var(m, types_core::INT2OID, 0), "40000")).unwrap()), None);
    assert_eq!(detexted(expr(m, &Utf8, eq(var(m, types_core::INT4OID, 0), "2147483648")).unwrap()), None);
    assert_eq!(detext_class(eq(x8(), "12")), Some(true));
    // not an integer argument, a NULL constant, another operator: no match
    assert_eq!(detext_class(eq(var(m, types_core::FLOAT8OID, 0), "1")), None);
    let null = Node::mk_const(m, gravity_decompose::catalog::TEXTOID, -1, DEFAULT_COLL, -1, Datum::from_usize(0), true, false).unwrap();
    assert_eq!(detext_class(op(m, 98, 67, DEFAULT_COLL, as_text(m, x8()), null)), None);
    assert_eq!(detext_class(op(m, 664, 740, DEFAULT_COLL, as_text(m, x8()), text(m, "7"))), None);
    assert_eq!(detext_value(types_core::INT8OID, b"12"), Some(Datum::from_i64(12)));
    assert_eq!(detext_value(types_core::INT4OID, b"012"), None);
}

/// Rule `identity` (D3078): an integer `x * 1`, `1 * x`, `x / 1`, `x + 0`, `0 + x` or `x - 0` is `x` itself; another
/// constant, a NULL, `0 - x`, `1 / x` and a mismatched type stay. The front key's `identity_class` says which.
#[test]
fn an_integer_identity_is_its_operand() {
    use gravity_decompose::normalize::identity_class;
    let ctx = MemoryContext::new("normalize");
    let m = ctx.mcx();
    let x = Node::mk_var(m, 1, 1, types_core::INT4OID, -1, 0, 0).unwrap();
    let k = |v: i32, null: bool| Node::mk_const(m, types_core::INT4OID, -1, 0, 4, Datum::from_i32(v), null, true).unwrap();
    // x * 1 (int4mul 514 / 141), 1 * x, x / 1 (int4div 528 / 154), x + 0 (int4pl 551 / 177), 0 + x, x - 0 (int4mi 555 / 181)
    for n in [arith(m, 514, 141, x, k(1, false)), arith(m, 514, 141, k(1, false), x), arith(m, 528, 154, x, k(1, false)),
        arith(m, 551, 177, x, k(0, false)), arith(m, 551, 177, k(0, false), x), arith(m, 555, 181, x, k(0, false))] {
        assert_eq!(identity_class(n), Some(true));
        assert!(!is_canonical(&NoFacts, n));
        assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(x), "the operand itself");
    }
    // the near forms keep their operator
    for n in [arith(m, 514, 141, x, k(2, false)), arith(m, 551, 177, x, k(1, false)), arith(m, 514, 141, x, k(1, true)),
        arith(m, 555, 181, k(0, false), x), arith(m, 528, 154, k(1, false), x)] {
        assert_ne!(identity_class(n), Some(true));
        assert!(expr(m, &NoFacts, n).unwrap().as_op_expr().is_some(), "an operator stays");
    }
    // int8: (x::int8) * 1::int8 (int8mul 686 / 465) is the cast itself
    let w = Node::mk_var(m, 1, 2, types_core::INT8OID, -1, 0, 0).unwrap();
    let one8 = Node::mk_const(m, types_core::INT8OID, -1, 0, 8, Datum::from_i64(1), false, true).unwrap();
    let args = NodeList::from_slice(m, &[w, one8]).unwrap();
    let n = Node::mk(m, OpExpr { opno: 686, opfuncid: 465, opresulttype: types_core::INT8OID, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap();
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(w));
    // (x::int8) * 1 (int84mul 690 / 1276, the int4 literal of `site_id::bigint * 1`) is the cast itself
    let args = NodeList::from_slice(m, &[w, k(1, false)]).unwrap();
    let n = Node::mk(m, OpExpr { opno: 690, opfuncid: 1276, opresulttype: types_core::INT8OID, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap();
    assert_eq!(identity_class(n), Some(true));
    assert!(expr(m, &NoFacts, n).unwrap().ptr_eq(w));
    // a cross-type form (int4 x under an int8 result) is not an identity of x's type
    let args = NodeList::from_slice(m, &[x, one8]).unwrap();
    let n = Node::mk(m, OpExpr { opno: 686, opfuncid: 465, opresulttype: types_core::INT8OID, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap();
    assert_eq!(identity_class(n), None);
}
