//! D2063 (V4R-578): `RelcacheFacts::index_opc` over mocked pg_amproc / pg_amop lists (SearchSysCacheList1 AMPROCNUM /
//! AMOPSTRATEGY): a key column passes only when a scan keyed by initdb operators reaches initdb functions alone; each
//! condition fails alone, a member over a user type does not fail its family, and a busy lock or a failed read is
//! `UNKNOWN`. (A lowering's read of these lists is its recorded dependency, C3 gravity_deps: no epoch here.)
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::Once;

use gravity_decompose::catalog::IndexOpc;
use gravity_decompose::Facts;
use gravity_hosts::facts::RelcacheFacts;
use mcx::{Mcx, MemoryContext, PgVec};
use syscache_seams::{PgAmopRow, PgAmprocRow};
use types_core::catalog::{BTREE_AM_OID, GIST_AM_OID, HASH_AM_OID};
use types_core::Oid;
use types_rel::{Relation, RelationData};
use types_tuple::tupdesc::CompactAttribute;
use types_tuple::{FormData_pg_attribute, TupleDescData};

const INT4: Oid = 23;
const INT8: Oid = 20;
const NUMERIC: Oid = 1700;
const BOX: Oid = 603;
const POINT_ARRAY: Oid = 1017;
const ANYARRAY: Oid = 2277;
const RECORD: Oid = 2249;

/// User objects (>= FirstNormalObjectId): regress equivclass.sql's int8alias1, a domain over int4 / box, a type with an
/// int4 cast WITHOUT FUNCTION, their procs and operators.
const INT8ALIAS1: Oid = 70020;
const DOM_INT4: Oid = 70030;
const ZB: Oid = 70050;
const ZT: Oid = 70060;
const USER_FN: Oid = 70010;
const USER_OP: Oid = 70011;
const USER_SORTFAM: Oid = 70012;
const USER_FAMILY: Oid = 70040;
const EXT_AM: Oid = 70070;

/// btree integer_ops as initdb leaves it (int4 / int8 members).
const INTEGER_OPS: Oid = 1976;
/// integer_ops after `ALTER OPERATOR FAMILY integer_ops USING btree ADD FUNCTION 1 (int4, numeric) USER_FN`.
const F_ADD_PROC: Oid = 9101;
/// ... `ADD OPERATOR 1 USER_OP (int4, numeric)`.
const F_ADD_OP: Oid = 9102;
/// ... after equivclass.sql: user operators and FUNCTION 1 over int8alias1.
const F_USER_TYPED: Oid = 9103;
/// ... an initdb operator registered over a user type.
const F_INITDB_OP_USER_TYPE: Oid = 9104;
/// ... an initdb ordering operator with a user sort family.
const F_USER_SORTFAM: Oid = 9105;
/// A family whose support procs are all over user types.
const F_NO_INITDB_PROC: Oid = 9106;
/// A family whose catalog read fails.
const F_READ_FAILS: Oid = 9107;
/// W2: integer_ops holding `zt_ops FOR TYPE zt` (FUNCTION 1 (zt, zt) and (zt, int4)).
const F_ZT: Oid = 9108;
const HASH_INTEGER_OPS: Oid = 1977;
/// W1: gist box_ops holding a `FOR TYPE zb` (a domain over box) opclass's user consistent function.
const GIST_BOX_OPS: Oid = 2593;
const ARRAY_OPS: Oid = 397;
const RECORD_OPS: Oid = 2994;

type Procs = &'static [(Oid, Oid, i16, Oid)];
type Ops = &'static [(Oid, Oid, i16, Oid, Oid)];

const INT_PROCS: Procs = &[(INT4, INT4, 1, 351), (INT4, INT4, 2, 3130), (INT4, INT8, 1, 2188), (INT8, INT8, 1, 842)];
const INT_OPS: Ops = &[(INT4, INT4, 1, 97, 0), (INT4, INT4, 3, 96, 0), (INT4, INT8, 1, 37, 0), (INT8, INT8, 1, 412, 0)];

fn family(fam: Oid) -> Option<(Vec<(Oid, Oid, i16, Oid)>, Vec<(Oid, Oid, i16, Oid, Oid)>)> {
    let with = |p: Procs, o: Ops| Some(([INT_PROCS, p].concat(), [INT_OPS, o].concat()));
    match fam {
        INTEGER_OPS | USER_FAMILY => with(&[], &[]),
        F_ADD_PROC => with(&[(INT4, NUMERIC, 1, USER_FN)], &[]),
        F_ADD_OP => with(&[], &[(INT4, NUMERIC, 1, USER_OP, 0)]),
        F_USER_TYPED => with(&[(INT8ALIAS1, INT8ALIAS1, 1, USER_FN), (INT8ALIAS1, INT8, 1, USER_FN)], &[(INT8ALIAS1, INT8ALIAS1, 1, USER_OP, 0), (INT8ALIAS1, INT8, 3, USER_OP, 0)]),
        F_INITDB_OP_USER_TYPE => with(&[], &[(INT4, INT8ALIAS1, 3, 96, 0)]),
        F_USER_SORTFAM => with(&[], &[(INT4, INT8, 6, 37, USER_SORTFAM)]),
        F_NO_INITDB_PROC => Some((vec![(INT8ALIAS1, INT8ALIAS1, 1, USER_FN)], INT_OPS.to_vec())),
        F_ZT => with(&[(ZT, ZT, 1, USER_FN), (ZT, INT4, 1, USER_FN)], &[(ZT, ZT, 1, USER_OP, 0), (ZT, INT4, 3, USER_OP, 0)]),
        HASH_INTEGER_OPS => Some((vec![(INT4, INT4, 1, 450), (INT4, INT4, 2, 425)], vec![(INT4, INT4, 1, 96, 0)])),
        GIST_BOX_OPS => Some((vec![(BOX, BOX, 1, 2578), (BOX, BOX, 2, 2583), (ZB, ZB, 1, USER_FN)], vec![(BOX, BOX, 3, 500, 0), (ZB, ZB, 3, USER_OP, 0)])),
        ARRAY_OPS => Some((vec![(ANYARRAY, ANYARRAY, 1, 382)], vec![(ANYARRAY, ANYARRAY, 1, 1072, 0)])),
        RECORD_OPS => Some((vec![(RECORD, RECORD, 1, 2987)], vec![(RECORD, RECORD, 1, 2990, 0)])),
        _ => None,
    }
}

/// An index: its AM and, per column, `(opcintype, stored type, family)`; `include` INCLUDE columns follow the keys.
struct Shape {
    relam: Oid,
    keys: &'static [(Oid, Oid, Oid)],
    include: usize,
}

const BUSY: Oid = 80099;

fn shape(indexid: Oid) -> Shape {
    let s = |relam: Oid, keys: &'static [(Oid, Oid, Oid)], include: usize| Shape { relam, keys, include };
    match indexid {
        80001 | BUSY => s(BTREE_AM_OID, &[(INT4, INT4, INTEGER_OPS)], 0),
        80002 => s(EXT_AM, &[(INT4, INT4, INTEGER_OPS)], 0),
        80003 => s(BTREE_AM_OID, &[(INT4, DOM_INT4, INTEGER_OPS)], 0),
        80004 => s(BTREE_AM_OID, &[(INT4, INT4, USER_FAMILY)], 0),
        80005 => s(BTREE_AM_OID, &[(INT4, INT4, 0)], 0),
        80006 => s(BTREE_AM_OID, &[(INT4, INT4, F_ADD_PROC)], 0),
        80007 => s(BTREE_AM_OID, &[(INT4, INT4, F_NO_INITDB_PROC)], 0),
        80008 => s(BTREE_AM_OID, &[(INT4, INT4, F_ADD_OP)], 0),
        80009 => s(BTREE_AM_OID, &[(INT4, INT4, F_USER_SORTFAM)], 0),
        80010 => s(BTREE_AM_OID, &[(INT4, INT4, F_INITDB_OP_USER_TYPE)], 0),
        80011 => s(BTREE_AM_OID, &[(INT4, INT4, F_USER_TYPED)], 0),
        80012 => s(BTREE_AM_OID, &[(INT4, INT4, F_READ_FAILS)], 0),
        80013 => s(GIST_AM_OID, &[(ZB, BOX, GIST_BOX_OPS)], 0),
        80014 => s(GIST_AM_OID, &[(BOX, BOX, GIST_BOX_OPS)], 0),
        80015 => s(BTREE_AM_OID, &[(ZT, INT4, F_ZT)], 0),
        80016 => s(BTREE_AM_OID, &[(INT4, INT4, F_ZT)], 0),
        80017 => s(BTREE_AM_OID, &[(ANYARRAY, POINT_ARRAY, ARRAY_OPS)], 0),
        80018 => s(BTREE_AM_OID, &[(RECORD, RECORD, RECORD_OPS)], 0),
        80019 => s(BTREE_AM_OID, &[(INT4, INT4, USER_FAMILY), (INT4, INT4, INTEGER_OPS)], 1),
        80020 => s(BTREE_AM_OID, &[(INT4, INT4, INTEGER_OPS), (INT8, INT8, INTEGER_OPS)], 1),
        80021 => s(HASH_AM_OID, &[(INT4, INT4, HASH_INTEGER_OPS)], 0),
        other => panic!("unknown index {other}"),
    }
}

fn tupdesc<'m>(mcx: Mcx<'m>, types: &[Oid]) -> TupleDescData<'m> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact_attrs = PgVec::new_in(mcx);
    for (i, &t) in types.iter().enumerate() {
        let att = FormData_pg_attribute { attnum: i as i16 + 1, atttypid: t, atttypmod: -1, attlen: 4, attbyval: true, attalign: types_tuple::TYPALIGN_INT, attstorage: types_tuple::TYPSTORAGE_PLAIN, ..Default::default() };
        compact_attrs.push(CompactAttribute::populate_from(&att));
        attrs.push(att);
    }
    TupleDescData { natts: types.len() as i32, tdtypeid: 0, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs, attrs }
}

fn index_data(mcx: Mcx<'_>, indexid: Oid) -> RelationData<'_> {
    let s = shape(indexid);
    let mut d = common::index_relation_data(mcx, false);
    d.rd_id = indexid;
    d.rd_rel.relam = s.relam;
    let (mut opcintype, mut opfamily, mut indkey) = (PgVec::new_in(mcx), PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut stored: Vec<Oid> = s.keys.iter().map(|k| k.1).collect();
    stored.extend(std::iter::repeat_n(INT4, s.include));
    for &(t, _, f) in s.keys {
        opcintype.push(t);
        opfamily.push(f);
    }
    for a in 0..stored.len() {
        indkey.push(a as i16 + 1);
    }
    d.rd_opcintype = opcintype;
    d.rd_opfamily = opfamily;
    d.rd_att = Rc::new(tupdesc(mcx, &stored));
    let i = d.rd_index.as_mut().unwrap();
    i.indexrelid = indexid;
    i.indnatts = stored.len() as i16;
    i.indnkeyatts = s.keys.len() as i16;
    i.indkey = indkey;
    d
}

fn install() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        relation_seams::relation_open::set(|mcx, relid, _lockmode| Ok(Relation::open(index_data(mcx, relid), None)));
        lmgr_seams::conditional_lock_relation_oid::set(|relid, _mode| Ok(relid != BUSY));
        lmgr_seams::unlock_relation_oid::set(|_relid, _mode| Ok(()));
        syscache_seams::lookup_pg_amproc_rows::set(|mcx, fam| {
            let Some((procs, _)) = family(fam) else { return Err(Box::new(types_error::PgError::error("cache lookup failed for operator family"))) };
            let mut v = PgVec::new_in(mcx);
            for (l, r, n, p) in procs {
                v.push(PgAmprocRow { amprocfamily: fam, amproclefttype: l, amprocrighttype: r, amprocnum: n, amproc: p });
            }
            Ok((v, true))
        });
        syscache_seams::lookup_pg_amop_rows::set(|mcx, fam| {
            let Some((_, ops)) = family(fam) else { return Err(Box::new(types_error::PgError::error("cache lookup failed for operator family"))) };
            let mut v = PgVec::new_in(mcx);
            for (l, r, s, o, sf) in ops {
                v.push(PgAmopRow { amopfamily: fam, amoplefttype: l, amoprighttype: r, amopstrategy: s, amoppurpose: if sf == 0 { b's' as i8 } else { b'o' as i8 }, amopopr: o, amopsortfamily: sf });
            }
            Ok((v, true))
        });
    });
    types_relscan::register_index_am(EXT_AM, types_relscan::IndexAmKind::Hnsw);
}

fn opc(indexid: Oid) -> IndexOpc {
    install();
    let ctx = MemoryContext::new("facts-index-opc");
    RelcacheFacts::new(ctx.mcx()).index_opc(indexid)
}

fn passes(indexid: Oid) -> bool {
    let o = opc(indexid);
    assert!(o.known && o.nkeys == 1, "index {indexid}: {o:?}");
    o.all()
}

#[test]
fn an_initdb_int4_btree_column_passes() {
    assert_eq!(opc(80001), IndexOpc { builtin: 1, nkeys: 1, known: true });
    assert!(opc(80001).all() && opc(80001).col(0));
    assert!(passes(80021), "a hash integer_ops column");
    assert!(passes(80014), "a gist box_ops column: the family's (zb, zb) rows are reached only by a zb key");
}

#[test]
fn each_condition_fails_alone() {
    assert!(!opc(80002).known, "(a) an extension AM's index is unknown");
    assert!(!passes(80003), "(b) a domain column: its stored type is a user type");
    assert!(!passes(80004), "(c) a user family of initdb procs");
    assert!(!passes(80005), "(c) no family");
    assert!(!passes(80006), "(d) ALTER OPERATOR FAMILY integer_ops ADD FUNCTION 1 (int4, numeric) <user function>");
    assert!(!passes(80007), "(d) no support proc over initdb types");
    assert!(!passes(80008), "(e) ALTER OPERATOR FAMILY integer_ops ADD OPERATOR 1 <user operator> (int4, numeric)");
    assert!(!passes(80009), "(e) an initdb ordering operator with a user sort family");
    assert!(!passes(80010), "(e) an initdb operator registered over a user type");
}

#[test]
fn a_member_over_a_user_type_does_not_fail_its_family() {
    assert!(passes(80011), "equivclass.sql's int8alias1 rows in integer_ops");
    assert!(passes(80016), "an int4_ops column of a family that also holds zt_ops's rows");
}

#[test]
fn a_user_or_dispatching_opcintype_fails() {
    assert!(!passes(80013), "W1: a gist opclass FOR TYPE zb (a domain over box) in box_ops, STORAGE box");
    assert!(!passes(80015), "W2: zt_ops FOR TYPE zt in integer_ops over an int4 column");
    assert!(!passes(80017), "W3: array_ops over point[] calls point's DEFAULT btree opclass through the typcache");
    assert!(!passes(80018), "record_ops calls each field type's DEFAULT opclass through the typcache");
}

#[test]
fn two_keys_and_an_include_column() {
    let o = opc(80019);
    assert_eq!((o.known, o.nkeys), (true, 2));
    assert!(!o.col(0) && o.col(1) && !o.col(2), "{o:?}");
    assert!(!o.all() && !o.through(0) && !o.through(1), "a user leading column fails every prefix");
    let o = opc(80020);
    assert_eq!(o, IndexOpc { builtin: 0b11, nkeys: 2, known: true });
    assert!(o.all() && o.through(1) && !o.through(2));
}

#[test]
fn a_busy_lock_or_a_failed_read_is_unknown() {
    assert_eq!(opc(BUSY), IndexOpc::UNKNOWN);
    assert_eq!(opc(80012), IndexOpc::UNKNOWN);
}
