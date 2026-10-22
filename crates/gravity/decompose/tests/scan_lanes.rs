// lane_of: every column class × access method gets the Rep and LaneLife of lowerv3 design §1.1's table.
use gravity_abi::ctx::lane;
use gravity_decompose::ops::scan::lane_of;
use gravity_decompose::ops::shared::scan::{text_zone_op, zone_op, zone_word_of};
use gravity_decompose::{Am, AttFact};
use gravity_program::{DictRef, Home, LaneLife, NumTypmod, NumForm, Rep, Until, ZoneOp};

fn att(typid: u32, typmod: i32, typlen: i16, byval: bool) -> AttFact {
    AttFact::plain(typid, typmod, typlen, byval, false)
}

fn numeric(p: i32, s: i32) -> AttFact {
    att(1700, ((p << 16) | (s & 0x7ff)) + 4, -1, false)
}

#[test]
fn by_value_columns_are_words_on_every_am() {
    for (typid, len) in [(21, 2), (23, 4), (20, 8), (1082, 4), (1114, 8), (26, 4), (700, 4), (701, 8), (16, 1)] {
        let h = lane_of(0, 1, att(typid, -1, len, true), Am::Heap);
        assert_eq!((h.rep, h.life, h.kind, h.width), (Rep::Word { bytes: 8 }, LaneLife::VALUE, lane::KIND_BYVAL, 8), "heap {typid}");
        let c = lane_of(0, 1, att(typid, -1, len, true), Am::Pgrcolumnar);
        assert_eq!((c.rep, c.life, c.width), (Rep::Word { bytes: len as u8 }, LaneLife::VALUE, len as u8), "pgrc {typid}");
    }
}

#[test]
fn pgrc_float_and_bool_keep_the_fixed_kind_byte_but_are_words() {
    for (typid, len) in [(700, 4), (701, 8), (16, 1)] {
        let c = lane_of(2, 3, att(typid, -1, len, true), Am::Pgrcolumnar);
        assert_eq!(c.kind, lane::KIND_FIXED, "the runtime lane block is unchanged");
        assert!(matches!(c.rep, Rep::Word { .. }), "D198: {typid} is never a packed numeric");
        assert_eq!(c.num, NumTypmod::NONE);
    }
}

#[test]
fn text_columns_are_page_pointers_on_heap_and_codes_on_pgrc() {
    for typid in [25, 1043, 1042] {
        let h = lane_of(3, 4, att(typid, -1, -1, false), Am::Heap);
        assert_eq!((h.rep, h.life), (Rep::VarlenaPtr, LaneLife::PAGE));
        assert_eq!(h.life, LaneLife { until: Until::Window, home: Home::PagePin });
        let c = lane_of(3, 4, att(typid, -1, -1, false), Am::Pgrcolumnar);
        assert_eq!(c.rep, Rep::Code { dict: DictRef { lane: 3, fp_table: DictRef::NO_TABLE } });
        assert_eq!(c.life, LaneLife { until: Until::Part, home: Home::Dictionary });
        assert_eq!((c.kind, c.width, c.region), (lane::KIND_CODES, 4, lane::REGION_DICTIONARY));
    }
}

#[test]
fn other_varlenas_are_pointers_with_the_source_life() {
    for typid in [17, 3802, 114] {
        let h = lane_of(0, 1, att(typid, -1, -1, false), Am::Heap);
        assert_eq!((h.rep, h.life), (Rep::VarlenaPtr, LaneLife::PAGE));
        let c = lane_of(0, 1, att(typid, -1, -1, false), Am::Pgrcolumnar);
        assert_eq!((c.rep, c.life, c.kind), (Rep::VarlenaPtr, LaneLife::PGRC, lane::KIND_BYREF));
    }
    let u = lane_of(0, 1, att(1700, -1, -1, false), Am::Pgrcolumnar);
    assert_eq!((u.rep, u.num), (Rep::VarlenaPtr, NumTypmod::NONE), "an unconstrained numeric has no typmod facts");
}

#[test]
fn fixed_length_by_reference_columns_are_never_varlenas() {
    for (typid, len) in [(19, 64), (2950, 16), (1186, 16), (1266, 12)] {
        let h = lane_of(0, 1, att(typid, -1, len, false), Am::Heap);
        assert_eq!((h.rep, h.life), (Rep::FixedPtr { len: len as u16 }, LaneLife::PAGE));
        let c = lane_of(0, 1, att(typid, -1, len, false), Am::Pgrcolumnar);
        assert_eq!((c.rep, c.life), (Rep::FixedPtr { len: len as u16 }, LaneLife::PGRC));
    }
    assert_eq!(lane_of(0, 1, att(2275, -1, -2, false), Am::Heap).rep, Rep::CstrPtr);
}

#[test]
fn numeric_typmod_facts_ride_the_lane() {
    let h = lane_of(0, 1, numeric(15, 2), Am::Heap);
    assert_eq!((h.rep, h.life), (Rep::VarlenaPtr, LaneLife::PAGE));
    assert_eq!(h.num, NumTypmod::of(15, 2));
    let c = lane_of(0, 1, numeric(15, 2), Am::Pgrcolumnar);
    assert_eq!(c.rep, Rep::PackedNum { facts: NumTypmod::of(15, 2), form: NumForm::Window });
    assert_eq!((c.kind, c.width, c.life), (lane::KIND_FIXED, 8, LaneLife::PGRC));
    let w = lane_of(0, 1, numeric(25, 2), Am::Pgrcolumnar);
    assert!(matches!(w.rep, Rep::PackedNum { facts, .. } if !facts.narrow()), "D197: p > 18 is wide");
}

/// D2700: a domain column keys by its base (`AttFact::base`): a domain over text reads codes on pgrc, a domain over
/// `numeric(p, s)` (the column's own typmod -1, the domain's p / s its base typmod) carries the facts and the packed form.
#[test]
fn a_domain_column_keys_by_its_base() {
    let dom = |base: u32, base_typmod: i32, typlen: i16| AttFact { base, base_typmod, ..att(70_001, -1, typlen, false) };
    for base in [25, 1043, 1042] {
        let d = dom(base, -1, -1);
        assert!(d.is_text(), "a domain over {base}");
        let c = lane_of(3, 4, d, Am::Pgrcolumnar);
        assert_eq!((c.rep, c.kind), (Rep::Code { dict: DictRef { lane: 3, fp_table: DictRef::NO_TABLE } }, lane::KIND_CODES));
        assert_eq!(lane_of(3, 4, d, Am::Heap).rep, Rep::VarlenaPtr);
    }
    let n = dom(1700, numeric(15, 2).typmod, -1);
    assert_eq!((n.numeric_scale(), n.num_facts()), (Some(2), NumTypmod::of(15, 2)));
    assert_eq!(lane_of(0, 1, n, Am::Heap).num, NumTypmod::of(15, 2));
    assert_eq!(lane_of(0, 1, n, Am::Pgrcolumnar).rep, Rep::PackedNum { facts: NumTypmod::of(15, 2), form: NumForm::Window });
    let other = dom(17, -1, -1);
    assert!(!other.is_text() && other.num_facts() == NumTypmod::NONE, "a domain over bytea is neither");
    let plain = att(25, -1, -1, false);
    assert_eq!((plain.base, plain.base_typmod), (25, -1), "a column that is no domain is its own base");
}

#[test]
fn num_facts_decode_the_typmod() {
    let f = |p: i32, s: i32| NumTypmod::of_typmod(((p << 16) | (s & 0x7ff)) + 4);
    assert_eq!(f(15, 2), NumTypmod::of(15, 2));
    assert_eq!(f(10, -3), NumTypmod::of(10, -3), "a negative scale is the 11-bit signed field");
    assert_eq!(NumTypmod::of_typmod(-1), NumTypmod::NONE);
    assert!(f(18, 18).narrow() && f(18, 0).narrow());
    assert!(!f(19, 2).narrow() && !f(10, -3).narrow() && !NumTypmod::NONE.narrow());
}

#[test]
fn rep_of_storage_follows_typlen_and_byval() {
    assert_eq!(Rep::of_storage(4, true), Rep::Word { bytes: 4 });
    assert_eq!(Rep::of_storage(-1, false), Rep::VarlenaPtr);
    assert_eq!(Rep::of_storage(-2, false), Rep::CstrPtr);
    assert_eq!(Rep::of_storage(16, false), Rep::FixedPtr { len: 16 });
}

#[test]
fn until_orders_shortest_first() {
    assert!(Until::Call < Until::Row && Until::Row < Until::Window && Until::Window < Until::Part);
    assert!(Until::Part < Until::Life && Until::Life < Until::Statement);
}

/// D257: a zone predicate compares the lane's stored integer with the constant's, so only operators whose operands share
/// the stored units map: `d = '2020-01-01'::timestamp` over a date lane compared days with microseconds and skipped every
/// granule. The cross-type date vs timestamp (2345..2350) and date vs timestamptz (2371..2376) operators map to none; the
/// same-type date, timestamp and timestamptz ones and the integer family across widths still map.
#[test]
fn d257_zone_op_maps_only_same_unit_operators() {
    for o in (2345..=2350).chain(2371..=2376) {
        assert_eq!(zone_op(o), None, "{o}");
    }
    let same = [(1093, ZoneOp::Eq), (1094, ZoneOp::Ne), (1095, ZoneOp::Lt), (1096, ZoneOp::Le), (1097, ZoneOp::Gt), (1098, ZoneOp::Ge)];
    let ts = [(2060, ZoneOp::Eq), (2061, ZoneOp::Ne), (2062, ZoneOp::Lt), (2063, ZoneOp::Le), (2064, ZoneOp::Gt), (2065, ZoneOp::Ge)];
    let tstz = [(1320, ZoneOp::Eq), (1321, ZoneOp::Ne), (1322, ZoneOp::Lt), (1323, ZoneOp::Le), (1324, ZoneOp::Gt), (1325, ZoneOp::Ge)];
    let ints = [(96, ZoneOp::Eq), (416, ZoneOp::Eq), (15, ZoneOp::Eq), (418, ZoneOp::Lt), (37, ZoneOp::Lt), (1864, ZoneOp::Lt)];
    for (o, z) in same.iter().chain(ts.iter()).chain(tstz.iter()).chain(ints.iter()) {
        assert_eq!(zone_op(*o), Some(*z), "{o}");
    }
}

/// SRC-09: text_zone_op maps text's own comparisons (pg_operator 664 `<`, 665 `<=`, 98 `=`, 667 `>=`, 666 `>` and `=`'s
/// negator 531 `<>`) and, where the btree table holds text_pattern_ops, its byte-order ones (2314 `~<~`, 2315 `~<=~`, 2317
/// `~>=~`, 2318 `~>~`), nothing else over the whole builtin range: never bpchar's (blank-padded: 1054 `=`, 1058 `<`),
/// name's (93, 660), an integer's (97) or a cross-type text operator.
#[test]
fn src09_text_zone_op_maps_text_comparisons_only() {
    let text = [(664, ZoneOp::Lt), (665, ZoneOp::Le), (98, ZoneOp::Eq), (667, ZoneOp::Ge), (666, ZoneOp::Gt), (531, ZoneOp::Ne)];
    for (o, z) in text {
        assert_eq!(text_zone_op(o), Some(z), "{o}");
    }
    let pattern = [(2314, ZoneOp::Lt), (2315, ZoneOp::Le), (2317, ZoneOp::Ge), (2318, ZoneOp::Gt)];
    for o in 0..=u32::from(u16::MAX) {
        let Some(z) = text_zone_op(o) else { continue };
        assert!(text.iter().chain(pattern.iter()).any(|&t| t == (o, z)), "pg_operator {o}: {z:?}");
    }
    for o in [1054, 1058, 93, 660, 97] {
        assert_eq!(text_zone_op(o), None, "{o}");
    }
}

/// OIDW-85..90: zone_op reads btree membership, `<>` as a btree `=`'s negator and the input types' stored units, and maps
/// over the whole builtin operator range exactly the operators its hand list named (kept here as the reference): the
/// integer family across widths, date / timestamp / timestamptz each with itself, text `=` / `<>`; and, the list's
/// gap, the other signed-word types with themselves: money (pg_operator 900..905) and time (1108..1113); and oid's
/// (607..612, NEW-47: an unsigned 4-byte word, its zones keyed zero-extended).
#[test]
fn zone_op_from_btree_families_maps_the_old_hand_list() {
    let old = |o: u32| {
        Some(match o {
            96 | 410 | 94 | 416 | 15 | 532 | 533 | 1862 | 1868 | 1093 | 2060 | 1320 | 98 => ZoneOp::Eq,
            518 | 411 | 519 | 417 | 36 | 538 | 539 | 1863 | 1869 | 1094 | 2061 | 1321 | 531 => ZoneOp::Ne,
            97 | 412 | 95 | 418 | 37 | 534 | 535 | 1864 | 1870 | 1095 | 2062 | 1322 => ZoneOp::Lt,
            523 | 414 | 522 | 420 | 80 | 540 | 541 | 1866 | 1872 | 1096 | 2063 | 1323 => ZoneOp::Le,
            521 | 413 | 520 | 419 | 76 | 536 | 537 | 1865 | 1871 | 1097 | 2064 | 1324 => ZoneOp::Gt,
            525 | 415 | 524 | 430 | 82 | 542 | 543 | 1867 | 1873 | 1098 | 2065 | 1325 => ZoneOp::Ge,
            900 | 1108 | 607 => ZoneOp::Eq,
            901 | 1109 | 608 => ZoneOp::Ne,
            902 | 1110 | 609 => ZoneOp::Lt,
            904 | 1111 | 611 => ZoneOp::Le,
            903 | 1112 | 610 => ZoneOp::Gt,
            905 | 1113 | 612 => ZoneOp::Ge,
            _ => return None,
        })
    };
    for o in 0..=u32::from(u16::MAX) {
        assert_eq!(zone_op(o), old(o), "pg_operator {o}");
    }
    assert_eq!((0..=u32::from(u16::MAX)).filter(|&o| zone_op(o).is_some()).count(), 92);
}

/// NEW-47: a constant meets a zone in the zone's key order: oid (and a reg* alias) zero-extended, as pgrc keys an
/// unsigned 4-byte word's zones, so 2^31 and 2^32 - 1 sort above 2^31 - 1; int4 and date sign-extended; int2 sign-extended.
#[test]
fn new47_zone_word_zero_extends_unsigned_words() {
    let (oid, regclass, int4, date, int2) = (26, 2205, 23, 1082, 21);
    assert_eq!(zone_word_of(oid, 4, 0x7fff_ffff), Some(2_147_483_647));
    assert_eq!(zone_word_of(oid, 4, 0x8000_0000), Some(2_147_483_648));
    assert_eq!(zone_word_of(oid, 4, 0xffff_ffff), Some(4_294_967_295));
    assert_eq!(zone_word_of(regclass, 4, 0xffff_ffff), Some(4_294_967_295));
    assert_eq!(zone_word_of(int4, 4, 0xffff_ffff), Some(-1));
    assert_eq!(zone_word_of(date, 4, 0x8000_0000), Some(-2_147_483_648));
    assert_eq!(zone_word_of(int2, 2, 0xffff), Some(-1));
    assert_eq!(zone_word_of(oid, 3, 1), None);
}
