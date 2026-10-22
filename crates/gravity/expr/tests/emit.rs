//! The emit conformance (docs/gravity/expr-compiler.md §2.5): an IR function around
//! `Compiled::emit` over a fixture row set, run through the interpreter and TPDE, compared with
//! `Compiled::eval` and the incumbent per row: value, validity, error. The seams and the tree
//! builders are the differential harness's (`tests/differential.rs`; a shared module is a need).

use std::sync::Once;

use datum::{Datum, NullableDatum};
use execexpr::{exec_eval_expr, exec_init_expr, EvalSlots};
use gravity_expr::agg::VecAgg;
use gravity_expr::emit::vec::{VecAcc, VecEmit};
use gravity_expr::emit::word::WordRefs;
use execexpr::{exec_init_expr_subplans_agg, AggBind};
use gravity_expr::agg::{Agg, AggState};
use gravity_expr::emit::{AggCell, ColsRef, EmitError, EmitState, LaneRef, ParamNeed, RowRefs, WholeRef};
use gravity_expr::{Compiled, Expr, PgCatalog, Region, Row};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::Oid;
use types_error::PgError;
use types_nodes::list::NodeList;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::{BoolExprType, BoolTestType, MinMaxOp, NullTestType, ParamKind};
use types_portal::params::{ParamBind, ParamExternData, PARAM_FLAG_CONST};
use types_slot::{SlotData, TupleSlotKind};
use types_tuple::{varatt, CompactAttribute, FormData_pg_attribute, PgTypeShape, TupleDescData, TYPSTORAGE_PLAIN};

/// The emits' temporaries (`RowRefs::scratch`, `WordRefs::scratch`): one context per test thread, kept for the run.
fn scratch() -> Mcx<'static> {
    thread_local! { static CX: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("emit-test scratch"))); }
    CX.with(|c| c.mcx())
}

const INT2OID: Oid = 21;
const INT4OID: Oid = 23;
const INT8OID: Oid = 20;
const OIDOID: Oid = 26;
const FLOAT4OID: Oid = 700;
const FLOAT8OID: Oid = 701;
const BOOLOID: Oid = 16;
const TEXTOID: Oid = 25;
const NAMEOID: Oid = 19;
const NAMEARRAYOID: Oid = 1003;
const CSTRINGOID: Oid = 2275;
const DATEOID: Oid = 1082;
/// `time`: an int8 word whose compare (`time_cmp`) is a tier-B by-value shim, the hosted MinMax / RowCompare road.
const TIMEOID: Oid = 1083;
const INT4ARRAYOID: Oid = 1007;
const JSONBOID: Oid = 3802;
const NUMERICOID: Oid = 1700;
const TIMESTAMPOID: Oid = 1114;
const INTERVALOID: Oid = 1186;

static SEAMS: Once = Once::new();

fn shape(typlen: i16, typbyval: bool, typalign: u8, typcollation: Oid) -> PgTypeShape {
    PgTypeShape { typlen, typbyval, typalign: typalign as i8, typstorage: if typlen == -1 { b'x' as i8 } else { TYPSTORAGE_PLAIN }, typcollation }
}

fn install_seams() {
    SEAMS.call_once(|| {
        miscinit_seams::get_user_id::set(|| 10);
        syscache_seams::pg_proc_proname::set(|_funcid| Ok(None));
        namespace_seams::is_temp_namespace::set(|_| false);
        syscache_seams::pg_type_typnamespace::set(|_| Ok(Some(11)));
        syscache_seams::pg_type_element_shape::set(|typid| Ok(match typid { INT4ARRAYOID => Some(INT4OID), NAMEARRAYOID => Some(NAMEOID), TEXTARRAYOID => Some(TEXTOID), _ => None }.map(|typelem| syscache_seams::PgTypeElementShape { typelem, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER })));
        aclchk_seams::object_aclcheck::set(|_classid, _objid, _roleid, _mode| Ok(0));
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(match typid {
                INT4OID | OIDOID | FLOAT4OID | DATEOID => Some(shape(4, true, b'i', 0)),
                INT2OID => Some(shape(2, true, b's', 0)),
                INT8OID | FLOAT8OID | TIMEOID | TIMESTAMPOID => Some(shape(8, true, b'd', 0)),
                BOOLOID => Some(shape(1, true, b'c', 0)),
                TEXTOID | TEXTARRAYOID => Some(shape(-1, false, b'i', 100)),
                INT4ARRAYOID | NAMEARRAYOID | JSONBOID | NUMERICOID => Some(shape(-1, false, b'i', 0)),
                NAMEOID => Some(shape(64, false, b'c', 950)),
                INTERVALOID => Some(shape(16, false, b'd', 0)),
                CSTRINGOID => Some(shape(-2, false, b'c', 0)),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_type_typcache_shape::set(|typid| {
            Ok(matches!(typid, INT4OID | OIDOID | TIMEOID).then(|| {
                let mut name = types_tuple::NameData::default();
                name.namestrcpy(match typid { INT4OID => "int4", OIDOID => "oid", _ => "time" });
                let (typlen, typalign) = if typid == TIMEOID { (8, b'd') } else { (4, b'i') };
                syscache_seams::PgTypeTypcacheShape { typname: name, typlen, typbyval: true, typalign: typalign as i8, typstorage: b'p' as i8, typtype: b'b' as i8, typisdefined: true, typrelid: 0, typsubscript: 0, typelem: 0, typarray: 0, typcollation: 0 }
            }))
        });
        syscache_seams::syscache_hash_value_typeoid::set(|typid| Ok(typid.wrapping_mul(0x9e3779b1)));
        // btree int4_ops (opclass 1978, family 1976, `<` 97, cmp 351) time_ops (1996 / 1996, `<` 1110, cmp 1107) and oid_ops (1981 / 1989, `<` 609, cmp 356)
        syscache_seams::lookup_pg_opclass_shape::set(|opclass| {
            Ok(match opclass {
                1978 => Some(syscache_seams::PgOpclassShape { opcmethod: types_core::BTREE_AM_OID, opcfamily: 1976, opcintype: INT4OID, opckeytype: types_core::InvalidOid }),
                1996 => Some(syscache_seams::PgOpclassShape { opcmethod: types_core::BTREE_AM_OID, opcfamily: 1996, opcintype: TIMEOID, opckeytype: types_core::InvalidOid }),
                1981 => Some(syscache_seams::PgOpclassShape { opcmethod: types_core::BTREE_AM_OID, opcfamily: 1989, opcintype: OIDOID, opckeytype: types_core::InvalidOid }),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_amop_by_operator::set(|opno, _purpose, opfamily| {
            Ok(match (opno, opfamily) {
                (97, 1976) => Some(syscache_seams::PgAmopShape { amopstrategy: 1, amopsortfamily: 0, amoplefttype: INT4OID, amoprighttype: INT4OID }),
                (1110, 1996) => Some(syscache_seams::PgAmopShape { amopstrategy: 1, amopsortfamily: 0, amoplefttype: TIMEOID, amoprighttype: TIMEOID }),
                (609, 1989) => Some(syscache_seams::PgAmopShape { amopstrategy: 1, amopsortfamily: 0, amoplefttype: OIDOID, amoprighttype: OIDOID }),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_amproc::set(|opfamily, _l, _r, procnum| Ok(match (opfamily, procnum) { (1976, 1) => 351, (1996, 1) => 1107, (1989, 1) => 356, _ => 0 }));
        syscache_seams::lookup_pg_operator_shape::set(|opno| {
            Ok(match opno {
                97 => Some(syscache_seams::PgOperatorShape { oprnamespace: 11, oprleft: INT4OID, oprright: INT4OID, oprresult: BOOLOID, oprcom: 521, oprnegate: 525, oprcode: 66, oprrest: 0, oprjoin: 0, oprcanmerge: false, oprcanhash: false }),
                1110 => Some(syscache_seams::PgOperatorShape { oprnamespace: 11, oprleft: TIMEOID, oprright: TIMEOID, oprresult: BOOLOID, oprcom: 1112, oprnegate: 1113, oprcode: 1102, oprrest: 0, oprjoin: 0, oprcanmerge: false, oprcanhash: false }),
                609 => Some(syscache_seams::PgOperatorShape { oprnamespace: 11, oprleft: OIDOID, oprright: OIDOID, oprresult: BOOLOID, oprcom: 610, oprnegate: 612, oprcode: 716, oprrest: 0, oprjoin: 0, oprcanmerge: false, oprcanhash: false }),
                _ => None,
            })
        });
        indexcmds_seams::get_default_opclass::set(|type_id, am_id| Ok(match (type_id, am_id) { (INT4OID, types_core::BTREE_AM_OID) => 1978, (TIMEOID, types_core::BTREE_AM_OID) => 1996, (OIDOID, types_core::BTREE_AM_OID) => 1981, _ => 0 }));
        let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
        mbutils::init_seams();
        fmgr_core::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        syscache_seams::pg_type_typtype::set(|typid| Ok(matches!(typid, INT4OID | BOOLOID | TEXTOID | NAMEOID | NAMEARRAYOID | INT2OID | INT8OID | OIDOID | FLOAT4OID | FLOAT8OID | INT4ARRAYOID | DATEOID | TIMEOID | CSTRINGOID | JSONBOID | NUMERICOID | INTERVALOID | TIMESTAMPOID).then_some(b'b' as i8)));
        syscache_seams::pg_type_base_shape::set(|typid| Ok(matches!(typid, INT4OID | BOOLOID | TEXTOID | NAMEOID | INT2OID | INT8OID | OIDOID | FLOAT4OID | FLOAT8OID | DATEOID | TIMEOID | CSTRINGOID | JSONBOID | NUMERICOID | INTERVALOID | TIMESTAMPOID).then_some(syscache_seams::PgTypeBaseShape { typtype: b'b' as i8, typbasetype: 0, typtypmod: -1, typelem: 0, typsubscript: 0 })));
        syscache_seams::pg_type_io_shape::set(|typid| {
            let mk = |typinput, typoutput, typlen, typbyval| syscache_seams::PgTypeIoShape { oid: typid, typinput, typoutput, typreceive: 0, typsend: 0, typmodin: 0, typmodout: 0, typelem: 0, typlen, typbyval, typalign: b'i' as i8, typdelim: b',' as i8, typisdefined: true };
            Ok(match typid {
                INT4OID => Some(mk(42, 43, 4, true)),
                INT8OID => Some(mk(460, 461, 8, true)),
                TEXTOID => Some(mk(46, 47, -1, false)),
                JSONBOID => Some(mk(3806, 3805, -1, false)),
                _ => None,
            })
        });
        detoast_seams::detoast_attr::set(|mcx, raw| {
            let mut v = mcx::vec_with_capacity_in(mcx, raw.len())?;
            mcx::vec_append_bytes(&mut v, raw)?;
            Ok(v)
        });
    });
}

// ---- the tree builders (the harness's) ----

fn scan_var(mcx: Mcx<'_>, attno: i16, typ: Oid) -> Node<'_> {
    Node::mk_var(mcx, 1, attno, typ, -1, if typ == TEXTOID { 950 } else { 0 }, 0).unwrap()
}

fn konst(mcx: Mcx<'_>, typ: Oid, len: i32, byval: bool, v: Option<Datum>) -> Node<'_> {
    Node::mk_const(mcx, typ, -1, if typ == TEXTOID { 950 } else { 0 }, len, v.unwrap_or(Datum::null()), v.is_none(), byval).unwrap()
}

fn int4(mcx: Mcx<'_>, v: Option<i32>) -> Node<'_> {
    konst(mcx, INT4OID, 4, true, v.map(Datum::from_i32))
}

fn int8(mcx: Mcx<'_>, v: Option<i64>) -> Node<'_> {
    konst(mcx, INT8OID, 8, true, v.map(Datum::from_i64))
}

fn float8(mcx: Mcx<'_>, v: Option<f64>) -> Node<'_> {
    konst(mcx, FLOAT8OID, 8, true, v.map(Datum::from_f64))
}

fn boolc(mcx: Mcx<'_>, v: Option<bool>) -> Node<'_> {
    konst(mcx, BOOLOID, 1, true, v.map(Datum::from_bool))
}

fn text_image<'m>(mcx: Mcx<'m>, s: &str) -> Datum {
    let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 4).unwrap();
    mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + 4)).unwrap();
    mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
    Datum::from_usize(img.leak().as_ptr() as usize)
}

fn text_const<'m>(mcx: Mcx<'m>, s: &str) -> Node<'m> {
    let d = text_image(mcx, s);
    Node::mk_const(mcx, TEXTOID, -1, 950, -1, d, false, false).unwrap()
}

fn cstring_const<'m>(mcx: Mcx<'m>, s: &str) -> Node<'m> {
    let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 1).unwrap();
    mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
    mcx::vec_append_bytes(&mut img, &[0]).unwrap();
    let d = Datum::from_usize(img.leak().as_ptr() as usize);
    Node::mk_const(mcx, CSTRINGOID, -1, 0, -2, d, false, false).unwrap()
}

fn opexpr<'m>(mcx: Mcx<'m>, opfuncid: Oid, resulttype: Oid, inputcollid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: 0, opfuncid, opresulttype: resulttype, opretset: false, opcollid: 0, inputcollid, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn funcexpr<'m>(mcx: Mcx<'m>, funcid: Oid, resulttype: Oid, inputcollid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid, funcresulttype: resulttype, funcretset: false, funcvariadic: false, funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, funccollid: 0, inputcollid, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn nullif<'m>(mcx: Mcx<'m>, opfuncid: Oid, resulttype: Oid, inputcollid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::NullIfExpr { opno: 0, opfuncid, opresulttype: resulttype, opretset: false, opcollid: 0, inputcollid, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn distinct<'m>(mcx: Mcx<'m>, opfuncid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::DistinctExpr { opno: 0, opfuncid, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn boolexpr<'m>(mcx: Mcx<'m>, op: BoolExprType, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::BoolExpr { boolop: op, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn param(mcx: Mcx<'_>, paramid: i32, typ: Oid) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::Param { paramkind: ParamKind::PARAM_EXTERN, paramid, paramtype: typ, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap()
}

fn minmax<'m>(mcx: Mcx<'m>, least: bool, args: &[Node<'m>]) -> Node<'m> {
    minmax_of(mcx, INT4OID, least, args)
}

fn minmax_of<'m>(mcx: Mcx<'m>, typ: Oid, least: bool, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::MinMaxExpr { minmaxtype: typ, minmaxcollid: 0, inputcollid: 0, op: if least { MinMaxOp::IS_LEAST } else { MinMaxOp::IS_GREATEST }, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn time_const(mcx: Mcx<'_>, v: Option<i64>) -> Node<'_> {
    konst(mcx, TIMEOID, 8, true, v.map(Datum::from_i64))
}

fn case_test(mcx: Mcx<'_>) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::CaseTestExpr { typeId: INT4OID, typeMod: -1, collation: 0 }).unwrap()
}

fn when<'m>(mcx: Mcx<'m>, cond: Node<'m>, result: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::CaseWhen { expr: Some(cond), result: Some(result), location: -1 }).unwrap()
}

fn case<'m>(mcx: Mcx<'m>, arg: Option<Node<'m>>, whens: &[Node<'m>], default: Option<Node<'m>>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::CaseExpr { casetype: INT4OID, casecollid: 0, arg, args: NodeList::from_slice(mcx, whens).unwrap(), defresult: default, location: -1 }).unwrap()
}

fn coalesce<'m>(mcx: Mcx<'m>, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4OID, coalescecollid: 0, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn null_test<'a>(mcx: Mcx<'a>, arg: Node<'a>, t: NullTestType) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::NullTest { arg: Some(arg), nulltesttype: t, argisrow: false, location: -1 }).unwrap()
}

/// A row-valued IS [NOT] NULL (`argisrow`).
fn null_test_row<'a>(mcx: Mcx<'a>, arg: Node<'a>, t: NullTestType) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::NullTest { arg: Some(arg), nulltesttype: t, argisrow: true, location: -1 }).unwrap()
}

/// `ROW(a, b)` as a `record` (the parser's column names).
fn row2<'a>(mcx: Mcx<'a>, a: Node<'a>, b: Node<'a>) -> Node<'a> {
    let names = [Node::mk_string(mcx, "f1").unwrap(), Node::mk_string(mcx, "f2").unwrap()];
    Node::mk(mcx, types_nodes::primnodes::RowExpr { args: NodeList::from_slice(mcx, &[a, b]).unwrap(), row_typeid: RECORDOID, row_format: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, colnames: NodeList::from_slice(mcx, &names).unwrap(), location: -1 }).unwrap()
}

fn bool_test<'a>(mcx: Mcx<'a>, arg: Node<'a>, t: BoolTestType) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::BooleanTest { arg: Some(arg), booltesttype: t, location: -1 }).unwrap()
}

fn relabel<'a>(mcx: Mcx<'a>, arg: Node<'a>, typ: Oid) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::RelabelType { arg, resulttype: typ, resulttypmod: -1, resultcollid: 0, relabelformat: types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap()
}

fn int4_array_const<'m>(mcx: Mcx<'m>, elems: Option<&[Option<i32>]>) -> Node<'m> {
    let Some(elems) = elems else { return konst(mcx, INT4ARRAYOID, -1, false, None) };
    let values: Vec<Datum> = elems.iter().map(|v| v.map_or(Datum::null(), Datum::from_i32)).collect();
    let nulls: Vec<bool> = elems.iter().map(|v| v.is_none()).collect();
    let dims = [elems.len() as i32];
    let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &dims, &[1], INT4OID, 4, true, b'i').unwrap();
    let d = Datum::from_usize(img.leak().as_ptr() as usize);
    Node::mk_const(mcx, INT4ARRAYOID, -1, 0, -1, d, false, false).unwrap()
}

/// A 64-byte `NameData` image (a fixed-length by-reference datum), leaked for the test's life.
fn name_image(s: &str) -> Datum {
    let mut b = Box::new([0u8; 64]);
    b[..s.len()].copy_from_slice(s.as_bytes());
    Datum::from_usize(Box::leak(b).as_ptr() as usize)
}

/// A `name[]` literal: fixed-length by-reference elements (typlen 64, align 'c'; D123 / D419).
fn name_array_const<'m>(mcx: Mcx<'m>, elems: &[Option<&str>]) -> Node<'m> {
    let values: Vec<Datum> = elems.iter().map(|v| v.map_or(Datum::null(), name_image)).collect();
    let nulls: Vec<bool> = elems.iter().map(|v| v.is_none()).collect();
    let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &[elems.len() as i32], &[1], NAMEOID, 64, false, b'c').unwrap();
    Node::mk_const(mcx, NAMEARRAYOID, -1, 0, -1, Datum::from_usize(img.leak().as_ptr() as usize), false, false).unwrap()
}

/// `name = ANY / ALL (name[])` (nameeq, a shim taking views): the element is viewed at name's typlen, never the array's.
fn name_saop<'m>(mcx: Mcx<'m>, use_or: bool, scalar: Node<'m>, array: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: 93, opfuncid: 62, hashfuncid: 0, negfuncid: 0, useOr: use_or, inputcollid: 950, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

/// `n` int4 values over the two columns: `a`, `b`, `a + b`, repeated (a null column nulls its values past word 0's).
fn wide<'m>(mcx: Mcx<'m>, n: usize) -> Vec<Node<'m>> {
    (0..n).map(|i| match i % 3 {
        0 => scan_var(mcx, 1, INT4OID),
        1 => scan_var(mcx, 2, INT4OID),
        _ => opexpr(mcx, 177, INT4OID, 0, &[scan_var(mcx, 1, INT4OID), scan_var(mcx, 2, INT4OID)]),
    }).collect()
}

/// `CASE WHEN $1 = 'abc' THEN ARRAY[$2, 1, $2, 3, ..] (97 values) END`: a 2-argument cell site and a 100-argument one.
fn mixed_width<'m>(m: Mcx<'m>) -> Node<'m> {
    let elems: Vec<Node<'_>> = (0..gravity_expr::nodes::func::MAX_MASKED).map(|i| if i % 2 == 0 { scan_var(m, 2, INT4OID) } else { int4(m, Some(i as i32)) }).collect();
    let eq = opexpr(m, 67, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "abc")]);
    case(m, None, &[when(m, eq, array_md(m, &elems, false))], Some(int4_array_const(m, None)))
}

/// `ARRAY[ARRAY[..], ..]` over int4 (`multidims`; one array type for every dimension, as PG plans it).
fn array_md<'m>(mcx: Mcx<'m>, elems: &[Node<'m>], multidims: bool) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ArrayExpr { array_typeid: INT4ARRAYOID, array_collid: 0, element_typeid: INT4OID, elements: NodeList::from_slice(mcx, elems).unwrap(), multidims, list_start: -1, list_end: -1, location: -1 }).unwrap()
}

fn saop<'m>(mcx: Mcx<'m>, use_or: bool, scalar: Node<'m>, array: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: 96, opfuncid: 65, hashfuncid: 0, negfuncid: 0, useOr: use_or, inputcollid: 0, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

fn row_compare<'m>(mcx: Mcx<'m>, cmptype: i32, l: &[Node<'m>], r: &[Node<'m>]) -> Node<'m> {
    row_compare_by(mcx, cmptype, 97, 1976, l, r)
}

fn row_compare_by<'m>(mcx: Mcx<'m>, cmptype: i32, opno: Oid, fam: Oid, l: &[Node<'m>], r: &[Node<'m>]) -> Node<'m> {
    let mut opnos = types_nodes::list::OidList::nil();
    let mut fams = types_nodes::list::OidList::nil();
    let mut colls = types_nodes::list::OidList::nil();
    for _ in 0..l.len() {
        opnos.lappend(mcx, opno).unwrap();
        fams.lappend(mcx, fam).unwrap();
        colls.lappend(mcx, 0).unwrap();
    }
    Node::mk(mcx, types_nodes::primnodes::RowCompareExpr { cmptype, opnos, opfamilies: fams, inputcollids: colls, largs: NodeList::from_slice(mcx, l).unwrap(), rargs: NodeList::from_slice(mcx, r).unwrap() }).unwrap()
}

// ---- the fixture rows: typed columns, every value a datum word ----

type Word = Option<u64>;

const RECORDOID: Oid = 2249;

fn att(typ: Oid) -> (i16, bool, u8) {
    match typ {
        INT2OID => (2, true, b's'),
        INT8OID | FLOAT8OID | TIMESTAMPOID | TIMEOID => (8, true, b'd'),
        BOOLOID => (1, true, b'c'),
        NAMEOID => (64, false, b'c'),
        TEXTOID | JSONBOID | NUMERICOID | INT4ARRAYOID | TEXTARRAYOID => (-1, false, b'i'),
        RECORDOID => (-1, false, b'd'),
        _ => (4, true, b'i'),
    }
}

fn desc<'m>(mcx: Mcx<'m>, types: &[Oid]) -> std::rc::Rc<TupleDescData<'m>> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    for (i, &t) in types.iter().enumerate() {
        let (attlen, attbyval, attalign) = att(t);
        let a = FormData_pg_attribute { attnum: (i + 1) as i16, atttypid: t, attlen, attbyval, attalign: attalign as i8, attstorage: if attlen == -1 { b'x' as i8 } else { TYPSTORAGE_PLAIN }, attcollation: if t == TEXTOID { 950 } else { 0 }, ..Default::default() };
        compact.push(CompactAttribute::populate_from(&a));
        attrs.push(a);
    }
    std::rc::Rc::new(TupleDescData { natts: types.len() as i32, tdtypeid: 0, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs: compact, attrs })
}

fn virtual_slot<'m>(mcx: Mcx<'m>, types: &[Oid], row: &[Word]) -> SlotData<'m> {
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::Virtual, Some(desc(mcx, types)));
    {
        let base = slot.base_mut();
        for (i, v) in row.iter().enumerate() {
            base.tts_values[i] = v.map_or(Datum::null(), Datum::from_u64);
            base.tts_isnull[i] = v.is_none();
        }
    }
    exectuples::exec_store_virtual_tuple(&mut slot);
    slot
}

#[derive(Clone, Debug)]
enum Outcome {
    Null,
    Value(Vec<u8>),
    /// (sqlstate, message) of the error; `None` where the arm cannot name it (a host's stashed error).
    Error(Option<(String, String)>),
}

impl PartialEq for Outcome {
    fn eq(&self, o: &Outcome) -> bool {
        match (self, o) {
            (Outcome::Null, Outcome::Null) => true,
            (Outcome::Value(a), Outcome::Value(b)) => a == b,
            (Outcome::Error(a), Outcome::Error(b)) => a.is_none() || b.is_none() || a == b,
            _ => false,
        }
    }
}

fn err_of(e: &PgError) -> Outcome {
    Outcome::Error(Some((String::from_utf8_lossy(&types_error::unpack_sqlstate(e.sqlstate())).into_owned(), e.message().to_string())))
}

/// The error a raise mask names (abi `status::raise`), as the driver reports it.
fn raised(mask: u64) -> Outcome {
    use gravity_abi::status::raise;
    Outcome::Error(raise::describe(raise::first(mask)).map(|(s, m)| (String::from_utf8_lossy(s).into_owned(), m.to_string())))
}

/// D2311: PostgreSQL's by-value int2 / int4 result as C reads it (DatumGetInt16 / DatumGetInt32), in the sign-extended
/// word the emitted code holds: a hash wrapper's own word is zero-extended (`UInt32GetDatum`). Every other side is
/// compared raw, so an evaluated or emitted int2 / int4 word left zero-extended is a mismatch.
fn pg_value(v: NullableDatum, typ: Oid) -> NullableDatum {
    match typ {
        INT2OID if !v.isnull => NullableDatum { value: Datum::from_i64(v.value.as_i64() as i16 as i64), isnull: false },
        INT4OID if !v.isnull => NullableDatum { value: Datum::from_i64(v.value.as_i64() as i32 as i64), isnull: false },
        _ => v,
    }
}

fn bytes_of(r: NullableDatum, typ: Oid) -> Outcome {
    if r.isnull {
        return Outcome::Null;
    }
    let (typlen, byval, _) = att(typ);
    if byval {
        return Outcome::Value(r.value.as_u64().to_ne_bytes().to_vec());
    }
    let p = r.value.as_usize() as *const u8;
    // SAFETY: a non-null by-reference result addresses a live image of its type.
    unsafe {
        let v = match typlen {
            -1 => {
                if varatt::varatt_is_1b(p) {
                    core::slice::from_raw_parts(p.add(1), varatt::varsize_1b(p) - 1)
                } else {
                    core::slice::from_raw_parts(p.add(4), varatt::varsize_4b(p) - 4)
                }
            }
            n => core::slice::from_raw_parts(p, n as usize),
        };
        Outcome::Value(v.to_vec())
    }
}

/// What the emitted function reads and writes (`#[repr(C)]`; its address is the ctx).
#[repr(C)]
struct Frame {
    values: *const u64,
    nulls: *const u8,
    nrows: u64,
    consts: *const u64,
    consts_null: *const u8,
    region: *mut Region,
    cells: *mut gravity_expr::emit::Cell,
    out_val: *mut u64,
    out_valid: *mut u8,
    out_err: *mut u64,
    /// The blessed `record` descriptor of the slot for a whole-row case (`Case::whole`), else null.
    whole_desc: *const TupleDescData<'static>,
}

const F_VALUES: i32 = 0;
const F_NULLS: i32 = 8;
const F_NROWS: i32 = 16;
const F_CONSTS: i32 = 24;
const F_CONSTS_NULL: i32 = 32;
const F_REGION: i32 = 40;
const F_CELLS: i32 = 48;
const F_OUT_VAL: i32 = 56;
const F_OUT_VALID: i32 = 64;
const F_OUT_ERR: i32 = 72;
const F_WHOLE_DESC: i32 = 80;

/// The function: hoist the constants, loop the rows, `emit` per row, store the three outputs.
fn build(c: &Compiled<'_>, natts: usize, nconsts: usize, word_root: bool, whole: bool) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("emit_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let values = b.load(Type::Ptr, ctx, F_VALUES);
    let nulls = b.load(Type::Ptr, ctx, F_NULLS);
    let nrows = b.load(Type::I64, ctx, F_NROWS);
    let cp = b.load(Type::Ptr, ctx, F_CONSTS);
    let cnp = b.load(Type::Ptr, ctx, F_CONSTS_NULL);
    let region = b.load(Type::Ptr, ctx, F_REGION);
    let cells = b.load(Type::Ptr, ctx, F_CELLS);
    let out_val = b.load(Type::Ptr, ctx, F_OUT_VAL);
    let out_valid = b.load(Type::Ptr, ctx, F_OUT_VALID);
    let out_err = b.load(Type::Ptr, ctx, F_OUT_ERR);
    let whole_desc = b.load(Type::I64, ctx, F_WHOLE_DESC);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let mut consts = Vec::new();
    let mut consts_null = Vec::new();
    for j in 0..nconsts {
        consts.push(b.load(Type::I64, cp, (j * 8) as i32));
        let n = b.load_zext(Type::I8, cnp, j as i32);
        consts_null.push(b.icmp(IntCond::Ne, n, zero));
    }
    let l_head = b.create_block(&[Type::I64]);
    let l_row = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let i = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, i, nrows);
    b.cond_br(done, (l_out, &[]), (l_row, &[]));
    b.switch_to(l_row);
    let rv = b.ptr_add(values, i, (natts * 8) as u8, 0);
    let rn = b.ptr_add(nulls, i, natts as u8, 0);
    let whole = whole.then_some(WholeRef { values: rv, nulls: rn, desc: whole_desc });
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Slot { values: rv, nulls: rn }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: Some(region), cells: Some(cells), shreds: &[], codes: &[], tables: &[], rec: None, aggs: &[], word_root, whole, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = c.emit(&mut b, &row)?;
    let ov = b.ptr_add(out_val, i, 8, 0);
    b.store(ov, 0, l.value);
    let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let ovp = b.ptr_add(out_valid, i, 1, 0);
    b.store(ovp, 0, valid8);
    let oep = b.ptr_add(out_err, i, 8, 0);
    b.store(oep, 0, l.err);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_head, &[i1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

struct Case {
    name: &'static str,
    types: Vec<Oid>,
    rows: Vec<Vec<Word>>,
    typ: Oid,
    build: Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>,
    extern_params: Vec<(Oid, Word)>,
    /// The root read as its integer word (`RowRefs::word_root`: a numeric `extract` key).
    word_root: bool,
    /// A whole-row case: the slot's `record` descriptor is blessed and handed to the emitted arm (`RowRefs::whole`).
    whole: bool,
}

fn tc(name: &'static str, types: &[Oid], rows: &[Vec<Word>], typ: Oid, build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> Case {
    Case { name, types: types.to_vec(), rows: rows.to_vec(), typ, build: Box::new(build), extern_params: Vec::new(), word_root: false, whole: false }
}

fn i4(v: i32) -> Word {
    Some(Datum::from_i32(v).as_u64())
}
fn i2(v: i16) -> Word {
    Some(Datum::from_i16(v).as_u64())
}
fn i8w(v: i64) -> Word {
    Some(v as u64)
}
fn f8(v: f64) -> Word {
    Some(v.to_bits())
}
fn bw(v: bool) -> Word {
    Some(v as u64)
}

struct Run {
    eval: Vec<Outcome>,
    incumbent: Vec<Outcome>,
    interp: Vec<Outcome>,
    tpde: Vec<Outcome>,
    nodes: usize,
}

fn extern_bind(params: &[(Oid, Word)]) -> Vec<ParamExternData> {
    params.iter().map(|(t, v)| ParamExternData { value: v.map_or(Datum::null(), Datum::from_u64), isnull: v.is_none(), pflags: PARAM_FLAG_CONST, ptype: *t }).collect()
}

fn run_case(c: &Case) -> Run {
    install_seams();
    let cx = MemoryContext::new("emit-test");
    let mcx = cx.mcx();
    let expr = (c.build)(mcx);
    let ext = extern_bind(&c.extern_params);
    let bind = if ext.is_empty() { ParamBind::NONE } else { ParamBind { extern_params: Some(ext.as_slice()), ..ParamBind::NONE } };
    let natts = c.types.len();
    let mut incumbent = Vec::new();
    for row in &c.rows {
        let mut state = exec_init_expr(mcx, Some(expr), bind).unwrap().unwrap();
        state.arm_result_mcx(mcx);
        let mut slot = virtual_slot(mcx, &c.types, row);
        let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
        incumbent.push(match exec_eval_expr(&mut state, &mut slots) {
            Ok(v) => bytes_of(pg_value(v, c.typ), c.typ),
            Err(e) => err_of(&e),
        });
    }
    let mut compiled = Expr::compile(lists(), expr, &PgCatalog, bind).unwrap_or_else(|e| panic!("{}: compile: {e}", c.name));
    let nodes = compiled.kinds().count();
    // every row's value stays in the region (the frame's too): ten 1664-column rows' cells and images take ~240 kB
    let mut buf = vec![0u8; 1 << 20];
    let mut region = Region::over(&mut buf);
    let mut eval = Vec::new();
    for row in &c.rows {
        let values: Vec<Datum> = row.iter().map(|v| v.map_or(Datum::null(), Datum::from_u64)).collect();
        let nulls: Vec<bool> = row.iter().map(|v| v.is_none()).collect();
        let (slot_desc, slot) = (desc(mcx, &c.types), virtual_slot(mcx, &c.types, row));
        // a whole-row case forms from the slot's descriptor (blessed `record`), as the incumbent does
        let r = if c.whole { Row::scan_slot(&values, &nulls, &c.types, &slot_desc, &slot) } else { Row::scan_typed(&values, &nulls, &c.types, 0) };
        eval.push(match compiled.eval(&r, &mut region) {
            Ok(v) => bytes_of(v, c.typ),
            Err(e) => err_of(&e),
        });
    }
    let needs = compiled.param_needs(scratch()).unwrap();
    let mut consts: Vec<u64> = Vec::new();
    let mut consts_null: Vec<u8> = Vec::new();
    for n in &needs {
        let d = match n {
            ParamNeed::Const(d) | ParamNeed::Elem(d) => *d,
            ParamNeed::Extern { id, .. } => {
                let (_, w) = c.extern_params[*id as usize - 1];
                NullableDatum { value: w.map_or(Datum::null(), Datum::from_u64), isnull: w.is_none() }
            }
            ParamNeed::Exec { .. } | ParamNeed::Deferred { .. } => NullableDatum::null(),
        };
        consts.push(d.value.as_u64());
        consts_null.push(d.isnull as u8);
    }
    if c.whole { compiled.emit_check_whole() } else { compiled.emit_check() }.unwrap_or_else(|e| panic!("{}: emit_check: {e:?}", c.name));
    let (ir, f) = build(&compiled, natts, needs.len(), c.word_root, c.whole).unwrap_or_else(|e| panic!("{}: emit: {e:?}", c.name));
    // the whole-row case's descriptor: the slot's, stamped `record` and blessed (the owner's fill in the lowerer)
    let mut whole_desc = desc(mcx, &c.types);
    if c.whole {
        let d = std::rc::Rc::get_mut(&mut whole_desc).unwrap();
        d.tdtypeid = RECORDOID;
        typcache::assign_record_type_typmod(d).unwrap();
    }
    let whole_desc: *const TupleDescData<'static> = if c.whole { std::rc::Rc::as_ptr(&whole_desc).cast() } else { core::ptr::null() };
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", c.name));
    let mut state: EmitState = compiled.emit_state(emit_cx());
    // the cells' buffer: `rebind_all` rewrites the cells in place, so the hosted arm below runs over the same address
    let cells = state.ptr();
    let nrows = c.rows.len();
    let flat_values: Vec<u64> = c.rows.iter().flat_map(|r| r.iter().map(|v| v.unwrap_or(0))).collect();
    let flat_nulls: Vec<u8> = c.rows.iter().flat_map(|r| r.iter().map(|v| v.is_none() as u8)).collect();
    let run = |code: gravity_program::Code| -> Vec<Outcome> {
        let mut out_val = vec![0u64; nrows];
        let mut out_valid = vec![0u8; nrows];
        let mut out_err = vec![0u64; nrows];
        let mut rbuf = vec![0u8; 1 << 20];
        let mut region = Region::over(&mut rbuf);
        let mut frame = Frame { values: flat_values.as_ptr(), nulls: flat_nulls.as_ptr(), nrows: nrows as u64, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), region: &mut region, cells, out_val: out_val.as_mut_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr(), whole_desc };
        // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
        let r = unsafe { code.call((&mut frame as *mut Frame).cast()) };
        assert_eq!(r, 0);
        (0..nrows)
            .map(|i| {
                if out_err[i] != 0 {
                    raised(out_err[i])
                } else if out_valid[i] == 0 {
                    Outcome::Null
                } else if c.word_root {
                    Outcome::Value(out_val[i].to_ne_bytes().to_vec())
                } else {
                    bytes_of(NullableDatum { value: Datum::from_u64(out_val[i]), isnull: false }, c.typ)
                }
            })
            .collect()
    };
    let tpde = run(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde: {}", c.name, e.message())));
    // The program as a hosted owner keeps it (lowerv4 hir `finish_keeping`): its lists moved into a
    // context (`Compiled::into_in`, an unbound program's) and the cells re-pointed (`EmitState::rebind_all`); the same code
    // over it answers as over the heap program
    let hosted = c.extern_params.is_empty().then(|| gravity_backend::mapped(&ir, f).unwrap());
    let interp = run(gravity_backend::interp(ir, f).unwrap());
    if let Some(code) = hosted {
        let root = mcx::PinnedContext::new(MemoryContext::new("emit-test hosted"));
        // SAFETY: `moved` drops before `root` (declared after it), and nothing else takes the handle
        let mut moved = unsafe { compiled.into_in(root.handle()) };
        state.rebind_all(&mut moved);
        assert_eq!(run(code), tpde, "{}: the hosted program's run", c.name);
    }
    Run { eval, incumbent, interp, tpde, nodes }
}

fn n() -> Word {
    None
}

fn cases() -> Vec<Case> {
    let tt: Vec<Vec<Word>> = vec![vec![i8w(43_200_000_000), i8w(7_200_000_000)], vec![i8w(7_200_000_000), i8w(43_200_000_000)], vec![i8w(43_200_000_000), i8w(43_200_000_000)], vec![n(), i8w(1)], vec![i8w(86_399_000_000), n()], vec![n(), n()], vec![i8w(0), i8w(0)]];
    let hh: Vec<Vec<Word>> = (-8..8).map(|v| vec![i4(v), i4(v * 1000 + 1)]).chain([vec![n(), i4(1)]]).collect();
    let ii: Vec<Vec<Word>> = vec![vec![i4(7), i4(3)], vec![i4(-7), i4(2)], vec![i4(i32::MIN), i4(-1)], vec![i4(i32::MAX), i4(1)], vec![i4(5), i4(0)], vec![n(), i4(1)], vec![i4(1), n()], vec![n(), n()], vec![i4(0), i4(0)], vec![i4(100), i4(100)]];
    let ll: Vec<Vec<Word>> = [7, -7, 32767, -32768, 32768, -32769, i32::MAX as i64, i32::MIN as i64, i32::MAX as i64 + 1, i32::MIN as i64 - 1, i64::MAX, i64::MIN].iter().map(|&v| vec![i8w(v), i8w(1)]).chain([vec![n(), i8w(1)]]).collect();
    let ss: Vec<Vec<Word>> = [7, -7, i16::MAX, i16::MIN, 0].iter().map(|&v| vec![i2(v), i2(1)]).chain([vec![n(), i2(1)]]).collect();
    let mut v = vec![
        tc("var", &[INT4OID, INT4OID], &ii, INT4OID, |m| scan_var(m, 2, INT4OID)),
        Case { whole: true, ..tc("whole row", &[INT4OID, INT4OID], &ii, RECORDOID, |m| scan_var(m, 0, RECORDOID)) },
        Case { whole: true, ..tc("whole row is not null", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test(m, scan_var(m, 0, RECORDOID), NullTestType::IS_NOT_NULL)) },
        // a row-valued IS [NOT] NULL, emitted (emit/scalar.rs `row_null_test`): every field decides, a NULL composite is NULL
        Case { whole: true, ..tc("whole row is null (row-valued)", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test_row(m, scan_var(m, 0, RECORDOID), NullTestType::IS_NULL)) },
        Case { whole: true, ..tc("whole row is not null (row-valued)", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test_row(m, scan_var(m, 0, RECORDOID), NullTestType::IS_NOT_NULL)) },
        tc("row(a, b) is null", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test_row(m, row2(m, scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)), NullTestType::IS_NULL)),
        tc("row(a, b) is not null", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test_row(m, row2(m, scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)), NullTestType::IS_NOT_NULL)),
        tc("row(a, a / b) is not null: the field raises first", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test_row(m, row2(m, scan_var(m, 1, INT4OID), opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])), NullTestType::IS_NOT_NULL)),
        tc("const", &[INT4OID, INT4OID], &ii, INT4OID, |m| int4(m, Some(42))),
        tc("const null", &[INT4OID, INT4OID], &ii, INT4OID, |m| int4(m, None)),
        tc("relabel", &[INT4OID, INT4OID], &ii, INT4OID, |m| relabel(m, scan_var(m, 1, INT4OID), INT4OID)),
        tc("int4pl", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4mi", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 181, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4mul", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 141, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4div", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4mod", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 156, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4um", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 212, INT4OID, 0, &[scan_var(m, 1, INT4OID)])),
        tc("int4abs", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 1251, INT4OID, 0, &[scan_var(m, 1, INT4OID)])),
        tc("int4eq", &[INT4OID, INT4OID], &ii, BOOLOID, |m| opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4lt", &[INT4OID, INT4OID], &ii, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4ge const", &[INT4OID, INT4OID], &ii, BOOLOID, |m| opexpr(m, 150, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(3))])),
        tc("int4larger", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 768, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("btint4cmp", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 351, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4and", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 1898, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("int4not", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 1901, INT4OID, 0, &[scan_var(m, 1, INT4OID)])),
        tc("int48 cast", &[INT4OID, INT4OID], &ii, INT8OID, |m| funcexpr(m, 481, INT8OID, 0, &[scan_var(m, 1, INT4OID)])),
        tc("i4toi2 cast", &[INT4OID, INT4OID], &ii, INT2OID, |m| funcexpr(m, 314, INT2OID, 0, &[scan_var(m, 1, INT4OID)])),
        // the all-digit casts open-coded (int84 / int82 raise out of range, int28 widens)
        tc("int84 cast", &[INT8OID, INT8OID], &ll, INT4OID, |m| funcexpr(m, 480, INT4OID, 0, &[scan_var(m, 1, INT8OID)])),
        tc("int82 cast", &[INT8OID, INT8OID], &ll, INT2OID, |m| funcexpr(m, 714, INT2OID, 0, &[scan_var(m, 1, INT8OID)])),
        tc("int28 cast", &[INT2OID, INT2OID], &ss, INT8OID, |m| funcexpr(m, 754, INT8OID, 0, &[scan_var(m, 1, INT2OID)])),
        tc("i4tod cast", &[INT4OID, INT4OID], &ii, FLOAT8OID, |m| funcexpr(m, 316, FLOAT8OID, 0, &[scan_var(m, 1, INT4OID)])),
        tc("int48pl mixed", &[INT4OID, INT4OID], &ii, INT8OID, |m| opexpr(m, 1278, INT8OID, 0, &[scan_var(m, 1, INT4OID), int8(m, Some(i64::MAX - 3))])),
        tc("int4shl (tier B shim)", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 1902, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("hashint4 (tier B shim)", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 450, INT4OID, 0, &[scan_var(m, 1, INT4OID)])),
        // D2311: a hash's int4 word read by an open-coded consumer (the direct shim call; -8..8 reach negative hashes)
        tc("hashint4 < 0 (D2311)", &[INT4OID, INT4OID], &hh, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[funcexpr(m, 450, INT4OID, 0, &[scan_var(m, 1, INT4OID)]), int4(m, Some(0))])),
        tc("hashint4 % 2 (D2311)", &[INT4OID, INT4OID], &hh, INT4OID, |m| opexpr(m, 156, INT4OID, 0, &[funcexpr(m, 450, INT4OID, 0, &[scan_var(m, 1, INT4OID)]), int4(m, Some(2))])),
        tc("hashint4 widened to int8 (D2311)", &[INT4OID, INT4OID], &hh, INT8OID, |m| funcexpr(m, 481, INT8OID, 0, &[funcexpr(m, 450, INT4OID, 0, &[scan_var(m, 1, INT4OID)])])),
        tc("hashint2 < hashint4 (D2311)", &[INT4OID, INT4OID], &hh, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[funcexpr(m, 449, INT4OID, 0, &[funcexpr(m, 314, INT2OID, 0, &[scan_var(m, 1, INT4OID)])]), funcexpr(m, 450, INT4OID, 0, &[scan_var(m, 2, INT4OID)])])),
        tc("not", &[INT4OID, INT4OID], &ii, BOOLOID, |m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])])),
        tc("and", &[INT4OID, INT4OID], &ii, BOOLOID, |m| boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]), opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(3))]), boolc(m, None)])),
        tc("or", &[INT4OID, INT4OID], &ii, BOOLOID, |m| boolexpr(m, BoolExprType::OR_EXPR, &[opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]), boolc(m, None), opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(3))])])),
        tc("and short-circuits the error", &[INT4OID, INT4OID], &ii, BOOLOID, |m| boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 144, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), opexpr(m, 147, BOOLOID, 0, &[opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)]), int4(m, Some(1))])])),
        tc("is null", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test(m, scan_var(m, 1, INT4OID), NullTestType::IS_NULL)),
        tc("is not null", &[INT4OID, INT4OID], &ii, BOOLOID, |m| null_test(m, scan_var(m, 2, INT4OID), NullTestType::IS_NOT_NULL)),
        tc("coalesce", &[INT4OID, INT4OID], &ii, INT4OID, |m| coalesce(m, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID), int4(m, Some(-9))])),
        tc("coalesce all null", &[INT4OID, INT4OID], &ii, INT4OID, |m| coalesce(m, &[int4(m, None), int4(m, None)])),
        tc("greatest", &[INT4OID, INT4OID], &ii, INT4OID, |m| minmax(m, false, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID), int4(m, Some(4))])),
        tc("least", &[INT4OID, INT4OID], &ii, INT4OID, |m| minmax(m, true, &[scan_var(m, 1, INT4OID), int4(m, None), scan_var(m, 2, INT4OID)])),
        // the compare is a tier-B by-value shim (`time_cmp`): the hosted road through the site's cell
        tc("greatest time (tier B compare shim)", &[TIMEOID, TIMEOID], &tt, TIMEOID, |m| minmax_of(m, TIMEOID, false, &[scan_var(m, 1, TIMEOID), scan_var(m, 2, TIMEOID), time_const(m, Some(43_200_000_000))])),
        tc("least time (tier B compare shim)", &[TIMEOID, TIMEOID], &tt, TIMEOID, |m| minmax_of(m, TIMEOID, true, &[scan_var(m, 1, TIMEOID), time_const(m, None), scan_var(m, 2, TIMEOID)])),
        tc("row compare < time (tier B compare shim)", &[TIMEOID, TIMEOID], &tt, BOOLOID, |m| row_compare_by(m, 1, 1110, 1996, &[scan_var(m, 1, TIMEOID), scan_var(m, 2, TIMEOID)], &[time_const(m, Some(43_200_000_000)), time_const(m, Some(7_200_000_000))])),
        tc("row compare >= time (tier B compare shim)", &[TIMEOID, TIMEOID], &tt, BOOLOID, |m| row_compare_by(m, 4, 1110, 1996, &[scan_var(m, 1, TIMEOID), scan_var(m, 2, TIMEOID)], &[time_const(m, Some(43_200_000_000)), time_const(m, None)])),
        tc("case searched", &[INT4OID, INT4OID], &ii, INT4OID, |m| case(m, None, &[when(m, opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), int4(m, Some(0))), when(m, opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(0))]), opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)]))], Some(int4(m, Some(-1))))),
        tc("case guards the division", &[INT4OID, INT4OID], &ii, INT4OID, |m| case(m, None, &[when(m, opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), int4(m, Some(0)))], Some(opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])))),
        tc("case arg form", &[INT4OID, INT4OID], &ii, INT4OID, |m| case(m, Some(scan_var(m, 1, INT4OID)), &[when(m, opexpr(m, 65, BOOLOID, 0, &[case_test(m), int4(m, Some(7))]), int4(m, Some(70))), when(m, opexpr(m, 65, BOOLOID, 0, &[case_test(m), int4(m, Some(5))]), int4(m, Some(50)))], Some(int4(m, None)))),
        tc("nullif", &[INT4OID, INT4OID], &ii, INT4OID, |m| nullif(m, 65, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("nullif null second", &[INT4OID, INT4OID], &ii, INT4OID, |m| nullif(m, 65, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, None)])),
        tc("nullif null first", &[INT4OID, INT4OID], &ii, INT4OID, |m| nullif(m, 65, INT4OID, 0, &[int4(m, None), scan_var(m, 2, INT4OID)])),
        tc("nullif const equal", &[INT4OID, INT4OID], &ii, INT4OID, |m| nullif(m, 65, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(7))])),
        tc("is distinct from", &[INT4OID, INT4OID], &ii, BOOLOID, |m| distinct(m, 65, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("= any", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), int4_array_const(m, Some(&[Some(7), None, Some(100)])))),
        tc("= all", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, false, scan_var(m, 1, INT4OID), int4_array_const(m, Some(&[Some(7), Some(7)])))),
        tc("= any empty", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), int4_array_const(m, Some(&[])))),
        tc("= any null array", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), int4_array_const(m, None))),
        // an ARRAY[...] operand is unrolled as a constant list is: every element evaluated (in order, its raise PG's) before
        // the compares
        tc("= any array[b, 7, null]", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), array_md(m, &[scan_var(m, 2, INT4OID), int4(m, Some(7)), int4(m, None)], false))),
        tc("= all array[b, a]", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, false, scan_var(m, 1, INT4OID), array_md(m, &[scan_var(m, 2, INT4OID), scan_var(m, 1, INT4OID)], false))),
        tc("= any array[a, a / b]: the element raises past a hit", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), array_md(m, &[scan_var(m, 1, INT4OID), opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])], false))),
        tc("row compare <", &[INT4OID, INT4OID], &ii, BOOLOID, |m| row_compare(m, 1, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)], &[int4(m, Some(7)), int4(m, Some(2))])),
        tc("row compare >=", &[INT4OID, INT4OID], &ii, BOOLOID, |m| row_compare(m, 4, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)], &[int4(m, Some(-7)), int4(m, None)])),
        tc("int4in (tier C fmgr)", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 42, INT4OID, 0, &[cstring_const(m, "1234")])),
        tc("int4in bad (tier C error)", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 42, INT4OID, 0, &[cstring_const(m, "12x")])),
    ];
    let bb: Vec<Vec<Word>> = vec![vec![bw(true), bw(false)], vec![bw(false), bw(true)], vec![n(), bw(true)], vec![bw(true), n()], vec![n(), n()]];
    for (name, t) in [("is true", BoolTestType::IS_TRUE), ("is not true", BoolTestType::IS_NOT_TRUE), ("is false", BoolTestType::IS_FALSE), ("is not false", BoolTestType::IS_NOT_FALSE), ("is unknown", BoolTestType::IS_UNKNOWN), ("is not unknown", BoolTestType::IS_NOT_UNKNOWN)] {
        v.push(tc(name, &[BOOLOID, BOOLOID], &bb, BOOLOID, move |m| bool_test(m, scan_var(m, 1, BOOLOID), t)));
    }
    v.push(tc("booleq", &[BOOLOID, BOOLOID], &bb, BOOLOID, |m| opexpr(m, 60, BOOLOID, 0, &[scan_var(m, 1, BOOLOID), scan_var(m, 2, BOOLOID)])));
    v.push(tc("boollt", &[BOOLOID, BOOLOID], &bb, BOOLOID, |m| opexpr(m, 56, BOOLOID, 0, &[scan_var(m, 1, BOOLOID), scan_var(m, 2, BOOLOID)])));
    let ff: Vec<Vec<Word>> = vec![vec![f8(1.5), f8(2.0)], vec![f8(f64::NAN), f8(1.0)], vec![f8(1.0), f8(f64::NAN)], vec![f8(f64::INFINITY), f8(1.0)], vec![f8(f64::MAX), f8(f64::MAX)], vec![f8(1.0), f8(0.0)], vec![f8(0.0), f8(0.0)], vec![f8(-0.0), f8(0.0)], vec![f8(1e-300), f8(1e-300)], vec![n(), f8(1.0)], vec![f8(f64::NAN), f8(f64::NAN)], vec![f8(f64::INFINITY), f8(f64::INFINITY)], vec![f8(-3.25), f8(-3.25)]];
    v.push(tc("float8pl", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 218, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("float8mi", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 219, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("float8mul", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 216, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("float8div", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 217, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("float8um", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 220, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID)])));
    v.push(tc("float8abs", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| funcexpr(m, 221, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID)])));
    for (name, f) in [("float8eq", 293), ("float8ne", 294), ("float8lt", 295), ("float8le", 296), ("float8gt", 297), ("float8ge", 298)] {
        v.push(tc(name, &[FLOAT8OID, FLOAT8OID], &ff, BOOLOID, move |m| opexpr(m, f, BOOLOID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    }
    v.push(tc("float8larger", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| funcexpr(m, 223, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("btfloat8cmp", &[FLOAT8OID, FLOAT8OID], &ff, INT4OID, |m| funcexpr(m, 355, INT4OID, 0, &[scan_var(m, 1, FLOAT8OID), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("dtof", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT4OID, |m| funcexpr(m, 312, FLOAT4OID, 0, &[scan_var(m, 1, FLOAT8OID)])));
    v.push(tc("float48pl", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 281, FLOAT8OID, 0, &[funcexpr(m, 312, FLOAT4OID, 0, &[scan_var(m, 1, FLOAT8OID)]), scan_var(m, 2, FLOAT8OID)])));
    v.push(tc("float8 const", &[FLOAT8OID, FLOAT8OID], &ff, FLOAT8OID, |m| opexpr(m, 216, FLOAT8OID, 0, &[scan_var(m, 1, FLOAT8OID), float8(m, Some(2.5))])));
    let ss: Vec<Vec<Word>> = vec![vec![i2(7), i8w(3)], vec![i2(i16::MAX), i8w(i64::MAX)], vec![i2(i16::MIN), i8w(-1)], vec![i2(-1), i8w(i64::MIN)], vec![n(), i8w(0)], vec![i2(5), i8w(0)], vec![i2(0), n()]];
    v.push(tc("int2pl", &[INT2OID, INT8OID], &ss, INT2OID, |m| opexpr(m, 176, INT2OID, 0, &[scan_var(m, 1, INT2OID), scan_var(m, 1, INT2OID)])));
    v.push(tc("int2mul", &[INT2OID, INT8OID], &ss, INT2OID, |m| opexpr(m, 152, INT2OID, 0, &[scan_var(m, 1, INT2OID), scan_var(m, 1, INT2OID)])));
    v.push(tc("int28pl", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 841, INT8OID, 0, &[scan_var(m, 1, INT2OID), scan_var(m, 2, INT8OID)])));
    v.push(tc("int8pl", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 463, INT8OID, 0, &[scan_var(m, 2, INT8OID), scan_var(m, 2, INT8OID)])));
    v.push(tc("int8mul", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 465, INT8OID, 0, &[scan_var(m, 2, INT8OID), scan_var(m, 2, INT8OID)])));
    v.push(tc("int8div", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 466, INT8OID, 0, &[scan_var(m, 2, INT8OID), int8(m, Some(-1))])));
    v.push(tc("int8div by col", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 466, INT8OID, 0, &[int8(m, Some(100)), scan_var(m, 2, INT8OID)])));
    v.push(tc("int8mod", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 945, INT8OID, 0, &[scan_var(m, 2, INT8OID), int8(m, Some(-1))])));
    v.push(tc("int8um", &[INT2OID, INT8OID], &ss, INT8OID, |m| opexpr(m, 462, INT8OID, 0, &[scan_var(m, 2, INT8OID)])));
    v.push(tc("int84 cast", &[INT2OID, INT8OID], &ss, INT4OID, |m| funcexpr(m, 480, INT4OID, 0, &[scan_var(m, 2, INT8OID)])));
    v.push(tc("int82 cast", &[INT2OID, INT8OID], &ss, INT2OID, |m| funcexpr(m, 714, INT2OID, 0, &[scan_var(m, 2, INT8OID)])));
    // GV-D2: one rounding to float4 (2^59 + 2^35 + 1 is 2^59 + 2^36; through float8 first it read 2^59)
    let s4: Vec<Vec<Word>> = [(1i64 << 59) + (1 << 35) + 1, -((1i64 << 59) + (1 << 35) + 1), (1i64 << 62) + (1 << 38) + 1, i64::MAX, i64::MIN, 16_777_217].iter().map(|&x| vec![i2(0), i8w(x)]).chain([vec![i2(0), n()]]).collect();
    v.push(tc("i8tof rounds once (GV-D2)", &[INT2OID, INT8OID], &s4, FLOAT4OID, |m| funcexpr(m, 652, FLOAT4OID, 0, &[scan_var(m, 2, INT8OID)])));
    v.push(tc("int28lt", &[INT2OID, INT8OID], &ss, BOOLOID, |m| opexpr(m, 1852, BOOLOID, 0, &[scan_var(m, 1, INT2OID), scan_var(m, 2, INT8OID)])));
    v.push(tc("int2 relabel oid lt", &[INT2OID, INT8OID], &ss, BOOLOID, |m| opexpr(m, 716, BOOLOID, 0, &[relabel(m, scan_var(m, 1, INT2OID), OIDOID), relabel(m, int4(m, Some(3)), OIDOID)])));
    v.push(tc("date_mi", &[INT2OID, INT8OID], &ss, INT4OID, |m| opexpr(m, 1140, INT4OID, 0, &[relabel(m, int4(m, Some(1000)), DATEOID), relabel(m, int4(m, Some(-7)), DATEOID)])));
    // D2349: date - date over -infinity / infinity (i32::MIN / i32::MAX) raises 22008 as PG's date_mi does
    let dd: Vec<Vec<Word>> = [(1000, -7), (i32::MAX, 0), (0, i32::MAX), (i32::MIN, 5), (3, i32::MIN), (i32::MAX, i32::MAX)].iter().map(|&(a, b)| vec![i4(a), i4(b)]).chain([vec![None, i4(i32::MAX)]]).collect();
    v.push(tc("date_mi over infinity (D2349)", &[DATEOID, DATEOID], &dd, INT4OID, |m| opexpr(m, 1140, INT4OID, 0, &[scan_var(m, 1, DATEOID), scan_var(m, 2, DATEOID)])));
    let tt: Vec<Vec<Word>> = {
        let cx = Box::leak(Box::new(MemoryContext::new("emit-test text")));
        let m = cx.mcx();
        vec![vec![Some(text_image(m, "abc").as_u64()), i4(3)], vec![Some(text_image(m, "").as_u64()), i4(0)], vec![None, i4(1)], vec![Some(text_image(m, "héllo").as_u64()), i4(5)]]
    };
    v.push(tc("texteq (tier B views)", &[TEXTOID, INT4OID], &tt, BOOLOID, |m| opexpr(m, 67, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "abc")])));
    v.push(tc("textlen (tier B view → value)", &[TEXTOID, INT4OID], &tt, INT4OID, |m| funcexpr(m, 1257, INT4OID, 950, &[scan_var(m, 1, TEXTOID)])));
    // D2311: hashtext's int4 word through the shim host (a view argument), read by an open-coded consumer
    let th: Vec<Vec<Word>> = {
        let cx = Box::leak(Box::new(MemoryContext::new("emit-test hashtext")));
        let m = cx.mcx();
        (0..16).map(|i| vec![Some(text_image(m, &format!("w{i}")).as_u64()), i4(i)]).chain([vec![None, i4(0)]]).collect()
    };
    v.push(tc("hashtext < 0 (D2311)", &[TEXTOID, INT4OID], &th, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[funcexpr(m, 400, INT4OID, 950, &[scan_var(m, 1, TEXTOID)]), int4(m, Some(0))])));
    v.push(tc("hashtext (D2311: the int4 word itself)", &[TEXTOID, INT4OID], &th, INT4OID, |m| funcexpr(m, 400, INT4OID, 950, &[scan_var(m, 1, TEXTOID)])));
    v.push(tc("nullif text by-ref", &[TEXTOID, INT4OID], &tt, TEXTOID, |m| nullif(m, 67, TEXTOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "abc")])));
    v.push(tc("nullif text by-ref literal first", &[TEXTOID, INT4OID], &tt, TEXTOID, |m| nullif(m, 67, TEXTOID, 950, &[text_const(m, "abc"), scan_var(m, 1, TEXTOID)])));
    v.push(tc("textcat (tier B by-ref result)", &[TEXTOID, INT4OID], &tt, TEXTOID, |m| opexpr(m, 1258, TEXTOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "!")])));
    // D3620 fix 4 (main 09:19:17Z (d)): a 2-argument cell site (texteq) and a 100-argument one (an array of 97: its head,
    // two mask words, the values) in one program: every cell is 100 words wide, and each call reads only its own
    v.push(tc("texteq then array of 97 (a 2- and a 100-argument site in one program)", &[TEXTOID, INT4OID], &tt, INT4ARRAYOID, |m| mixed_width(m)));
    let nn: Vec<Vec<Word>> = vec![vec![Some(name_image("pg_catalog").as_u64()), i4(1)], vec![Some(name_image("public").as_u64()), i4(2)], vec![Some(name_image("other").as_u64()), i4(3)], vec![None, i4(4)]];
    v.push(tc("name = any (fixed-length by-ref elements, D419)", &[NAMEOID, INT4OID], &nn, BOOLOID, |m| name_saop(m, true, scan_var(m, 1, NAMEOID), name_array_const(m, &[Some("pg_catalog"), Some("public")]))));
    v.push(tc("name <> all with a null element (D419)", &[NAMEOID, INT4OID], &nn, BOOLOID, |m| {
        let a = name_array_const(m, &[Some("public"), None]);
        Node::mk(m, types_nodes::primnodes::ScalarArrayOpExpr { opno: 643, opfuncid: 659, hashfuncid: 0, negfuncid: 0, useOr: false, inputcollid: 950, args: NodeList::from_slice(m, &[scan_var(m, 1, NAMEOID), a]).unwrap(), location: -1 }).unwrap()
    }));
    v.push(tc("array multidims (V4R-85)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| {
        let (a, b) = (scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID));
        let (r1, r2) = (array_md(m, &[a, b], false), array_md(m, &[b, a], false));
        array_md(m, &[r1, r2], true)
    }));
    v.push(tc("array multidims with a null sub-array (V4R-85)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| {
        let r1 = array_md(m, &[scan_var(m, 1, INT4OID), int4(m, Some(5))], false);
        array_md(m, &[r1, int4_array_const(m, None)], true)
    }));
    // item 12: a constructor past one mask word (64 values): the null mask is a word per 64 values, up to a call's arguments
    v.push(tc("array of 70 (two mask words)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| array_md(m, &wide(m, 70), false)));
    v.push(tc("array of 97 (the most a masked site carries)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| array_md(m, &wide(m, gravity_expr::nodes::func::MAX_MASKED), false)));
    // PS4-10: past one site's 97 elements the mask words and elements ride continuation cells, as a wide row's
    for n in [98usize, 250, 1000] {
        v.push(tc(Box::leak(format!("array of {n} (continuation cells)").into_boxed_str()), &[INT4OID, INT4OID], &ii, INT4ARRAYOID, move |m| array_md(m, &wide(m, n), false)));
    }
    v.push(tc("array multidims of 130 sub-arrays (continuation cells)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| {
        let subs: Vec<Node<'_>> = (0..130).map(|_| array_md(m, &wide(m, 3), false)).collect();
        array_md(m, &subs, true)
    }));
    v.push(tc("array multidims of 130 sub-arrays, NULL ones (continuation cells)", &[INT4OID, INT4OID], &ii, INT4ARRAYOID, |m| {
        let subs: Vec<Node<'_>> = (0..130).map(|i| if i % 50 == 7 { int4_array_const(m, None) } else { array_md(m, &wide(m, 3), false) }).collect();
        array_md(m, &subs, true)
    }));
    v.push(tc("= any array of 120 (walked over continuation cells)", &[INT4OID, INT4OID], &ii, BOOLOID, |m| saop(m, true, scan_var(m, 1, INT4OID), array_md(m, &wide(m, 120), false))));
    v.push(tc("row of 70 (two mask words)", &[INT4OID, INT4OID], &ii, RECORDOID, |m| {
        // a RECORD row names each column (the parser's f1..fN), as execexpr's tuple descriptor needs
        let names: Vec<Node<'_>> = (1..=70).map(|i| Node::mk_string(m, Box::leak(format!("f{i}").into_boxed_str())).unwrap()).collect();
        Node::mk(m, types_nodes::primnodes::RowExpr { args: NodeList::from_slice(m, &wide(m, 70)).unwrap(), row_typeid: RECORDOID, row_format: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, colnames: NodeList::from_slice(m, &names).unwrap(), location: -1 }).unwrap()
    }));
    // CR-WIDE-ROW: past one site's 97 values the mask words and values ride continuation cells (98: one; 250: three, the
    // mask's 4 words and the values crossing cell boundaries; 1664: PG's widest target list)
    for n in [98usize, 250, 1664] {
        v.push(tc(Box::leak(format!("row of {n} (continuation cells)").into_boxed_str()), &[INT4OID, INT4OID], &ii, RECORDOID, move |m| {
            let names: Vec<Node<'_>> = (1..=n).map(|i| Node::mk_string(m, Box::leak(format!("f{i}").into_boxed_str())).unwrap()).collect();
            Node::mk(m, types_nodes::primnodes::RowExpr { args: NodeList::from_slice(m, &wide(m, n)).unwrap(), row_typeid: RECORDOID, row_format: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, colnames: NodeList::from_slice(m, &names).unwrap(), location: -1 }).unwrap()
        }));
    }
    let mut p = tc("param extern", &[INT4OID, INT4OID], &ii, INT4OID, |m| opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), param(m, 1, INT4OID)]));
    p.extern_params = vec![(INT4OID, i4(1000))];
    v.push(p);
    let mut p = tc("param extern null", &[INT4OID, INT4OID], &ii, INT4OID, |m| coalesce(m, &[param(m, 1, INT4OID), scan_var(m, 1, INT4OID)]));
    p.extern_params = vec![(INT4OID, None)];
    v.push(p);
    v
}

/// D3620 fix 4 (main 09:19:17Z (d)): a program's cells hold its widest site's words (`emit::cell_width`), not `MAX_ARGS`'s
/// 100 in every cell. A program of one 2-argument cell site: 2. A wide row's or array's continuation sites are `MAX_ARGS`
/// wide, so their program keeps 100. A program mixing a 2-argument site and a 100-argument one: 100 for both (the
/// conformance cases run each of these through the emitted code against the interpreter).
#[test]
fn cells_are_as_wide_as_the_widest_site() {
    install_seams();
    let cx = MemoryContext::new("cell-width");
    let m = cx.mcx();
    let width = |e: Node<'_>| {
        let mut c = Expr::compile(lists(), e, &PgCatalog, ParamBind::NONE).unwrap();
        let st = c.emit_state(emit_cx());
        assert_eq!(st.width(), gravity_expr::emit::cell_width(&c.prog));
        st.width()
    };
    let max = gravity_expr::nodes::func::MAX_ARGS;
    assert_eq!(width(opexpr(m, 67, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "abc")])), 2, "texteq alone");
    let names = |n: usize| -> Vec<Node<'_>> { (1..=n).map(|i| Node::mk_string(m, Box::leak(format!("f{i}").into_boxed_str())).unwrap()).collect() };
    let row = |n: usize| Node::mk(m, types_nodes::primnodes::RowExpr { args: NodeList::from_slice(m, &wide(m, n)).unwrap(), row_typeid: RECORDOID, row_format: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, colnames: NodeList::from_slice(m, &names(n)).unwrap(), location: -1 }).unwrap();
    assert_eq!(width(row(70)), 73, "a row of 70: its head, two mask words and the values");
    assert_eq!(width(row(98)), max, "a row past one site keeps MAX_ARGS (its continuation sites)");
    assert_eq!(width(array_md(m, &wide(m, 250), false)), max, "an array past one site keeps MAX_ARGS");
    assert_eq!(width(mixed_width(m)), max, "a 2-argument and a 100-argument site");
    assert_eq!(gravity_expr::emit::cell_size(2), 80, "a 2-wide cell: the 56-byte header, 2 words and 2 null bytes");
    assert_eq!(gravity_expr::emit::cell_size(max), 960, "was 952 for every cell");
}

/// Tier 0 (the core lane's `eval`) against the incumbent, written as a need in expr-compiler.md
/// §2.5: `x = ANY('{}')` with a null `x` is false in C (the empty array is tested first).
const KNOWN_EVAL_DEVIATIONS: &[&str] = &["= any empty"];

#[test]
fn emit_matches_eval_the_incumbent_the_interpreter_and_tpde() {
    let mut total = 0;
    let mut identical = 0;
    let mut nodes = 0;
    let mut bad = Vec::new();
    for c in cases() {
        let r = run_case(&c);
        nodes += r.nodes;
        for i in 0..c.rows.len() {
            total += 1;
            let same = r.incumbent[i] == r.interp[i] && r.incumbent[i] == r.tpde[i] && (r.eval[i] == r.incumbent[i] || KNOWN_EVAL_DEVIATIONS.contains(&c.name));
            if same {
                identical += 1;
            } else {
                bad.push(format!("{} row {i} {:?}: eval {:?} incumbent {:?} interp {:?} tpde {:?}", c.name, c.rows[i], r.eval[i], r.incumbent[i], r.interp[i], r.tpde[i]));
            }
        }
    }
    eprintln!("emit conformance: {total} cases, {identical} identical, {nodes} nodes emitted");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

// ---- the vector word form over lanes ----

/// The lanes fixture: the emitted function takes `(ctx)` = the frame below, runs `emit_word` over
/// every full word and the scalar `emit` over every row, and stores both masks.
#[repr(C)]
struct LaneFrame {
    lanes: [gravity_abi::ctx::Lane; 2],
    rows: u64,
    consts: *const u64,
    consts_null: *const u8,
    out_vec: *mut u64,
    out_row: *mut u64,
    ok: *mut u64,
}

fn lane_word_program(c: &Compiled<'_>, widths: [u8; 2], nconsts: usize) -> (gravity_ir::Program, gravity_ir::Func) {
    use gravity_abi::ctx::lane;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("emit_word_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let lanes_off = 0i32;
    let base_after = (2 * lane::SIZE) as i32;
    let rows = b.load(Type::I64, ctx, base_after);
    let cp = b.load(Type::Ptr, ctx, base_after + 8);
    let cnp = b.load(Type::Ptr, ctx, base_after + 16);
    let out_vec = b.load(Type::Ptr, ctx, base_after + 24);
    let out_row = b.load(Type::Ptr, ctx, base_after + 32);
    let okp = b.load(Type::Ptr, ctx, base_after + 40);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let mut consts = Vec::new();
    let mut consts_null = Vec::new();
    for j in 0..nconsts {
        consts.push(b.load(Type::I64, cp, (j * 8) as i32));
        let n = b.load_zext(Type::I8, cnp, j as i32);
        consts_null.push(b.icmp(IntCond::Ne, n, zero));
    }
    let mut lanes: Vec<Option<LaneRef>> = Vec::new();
    for (l, &w) in widths.iter().enumerate() {
        let off = lanes_off + (l * lane::SIZE) as i32;
        let ptr = b.load(Type::Ptr, ctx, off + lane::PTR as i32);
        let valid = b.load(Type::Ptr, ctx, off + lane::VALID as i32);
        lanes.push(Some(LaneRef { ptr, valid, valid_null: None, width: w, kind: lane::KIND_BYVAL, all_valid: false }));
    }
    let w = WordRefs { scratch: scratch(), lanes: &lanes, consts: &consts, consts_null: &consts_null, tables: &[] };
    let ok = c.emit_word_ok(&mut b, &w).unwrap();
    b.store(okp, 0, ok);
    let six = b.iconst(Type::I64, 6);
    let words = b.int_bin(IntBinOp::Ushr, rows, six);
    let l_head = b.create_block(&[Type::I64]);
    let l_word = b.create_block(&[]);
    let l_rows = b.create_block(&[Type::I64, Type::I64]);
    let l_row = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.mark_loop_header(l_rows);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let wi = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, wi, words);
    b.cond_br(done, (l_out, &[]), (l_word, &[]));
    b.switch_to(l_word);
    let base = b.int_bin(IntBinOp::Shl, wi, six);
    let m = c.emit_word(&mut b, &w, base).unwrap();
    let mp = b.ptr_add(out_vec, wi, 8, 0);
    b.store(mp, 0, m);
    b.br(l_rows, &[zero, zero]);
    b.switch_to(l_rows);
    let (i, acc) = (b.block_params(l_rows)[0], b.block_params(l_rows)[1]);
    let m64 = b.iconst(Type::I64, 64);
    let rdone = b.icmp(IntCond::Uge, i, m64);
    let l_store = b.create_block(&[]);
    b.cond_br(rdone, (l_store, &[]), (l_row, &[]));
    b.switch_to(l_store);
    let rp = b.ptr_add(out_row, wi, 8, 0);
    b.store(rp, 0, acc);
    let wi1 = b.int_bin(IntBinOp::Add, wi, one);
    b.br(l_head, &[wi1]);
    b.switch_to(l_row);
    let ri = b.int_bin(IntBinOp::Add, base, i);
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: ri, lanes: &lanes }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: None, cells: None, shreds: &[], codes: &[], tables: &[], rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = c.emit(&mut b, &row).unwrap();
    let t = b.icmp(IntCond::Ne, l.value, zero);
    let hit = b.int_bin(IntBinOp::And, l.valid, t);
    let bit = b.conv(ConvOp::Zext, Type::I64, hit);
    let sh = b.int_bin(IntBinOp::Shl, bit, i);
    let acc2 = b.int_bin(IntBinOp::Or, acc, sh);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_rows, &[i1, acc2]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    (pb.finish(), f)
}

#[test]
fn emit_word_matches_the_scalar_rows_and_eval() {
    install_seams();
    let cx = MemoryContext::new("emit-word-test");
    let mcx = cx.mcx();
    const ROWS: usize = 256;
    let a: Vec<i32> = (0..ROWS).map(|i| ((i as i64 * 7919) % 1000) as i32 - 200).collect();
    let bcol: Vec<i64> = (0..ROWS).map(|i| (i as i64 * 31) % 9 - 3).collect();
    let av: Vec<u64> = (0..ROWS / 64).map(|w| if w == 1 { 0xF0F0_F0F0_F0F0_F0F0 } else { !0 }).collect();
    let bv: Vec<u64> = vec![!0; ROWS / 64];
    let lanes = [gravity_abi::ctx::Lane::byval(a.as_ptr().cast(), av.as_ptr(), 4, 0, 0), gravity_abi::ctx::Lane::byval(bcol.as_ptr().cast(), bv.as_ptr(), 8, 0, 0)];
    let shapes: Vec<(&str, Box<dyn Fn(Mcx<'_>) -> Node<'_>>)> = vec![
        ("a >= 100 and a <= 500 and b <> 3", Box::new(|m| boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 150, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(100))]), opexpr(m, 149, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(500))]), opexpr(m, 468, BOOLOID, 0, &[scan_var(m, 2, INT8OID), int8(m, Some(3))])]))),
        ("a in (7, 300) or a = -3", Box::new(|m| boolexpr(m, BoolExprType::OR_EXPR, &[saop(m, true, scan_var(m, 1, INT4OID), int4_array_const(m, Some(&[Some(7), Some(300)]))), opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(-3))])]))),
        ("5 < a (commuted)", Box::new(|m| opexpr(m, 66, BOOLOID, 0, &[int4(m, Some(5)), scan_var(m, 1, INT4OID)]))),
        ("a = 3000000000 (does not fit: ok = 0)", Box::new(|m| opexpr(m, 852, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int8(m, Some(3_000_000_000))]))),
    ];
    for (name, build) in shapes {
        let expr = build(mcx);
        let compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
        let needs = compiled.param_needs(scratch()).unwrap();
        let consts: Vec<u64> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.value.as_u64(), _ => 0 }).collect();
        let consts_null: Vec<u8> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.isnull as u8, _ => 1 }).collect();
        let (ir, f) = lane_word_program(&compiled, [4, 8], needs.len());
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let expect_ok = !name.contains("does not fit");
        for (arm, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{name}: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
            let mut out_vec = vec![0u64; ROWS / 64];
            let mut out_row = vec![0u64; ROWS / 64];
            let mut ok = 7u64;
            let mut frame = LaneFrame { lanes, rows: ROWS as u64, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), out_vec: out_vec.as_mut_ptr(), out_row: out_row.as_mut_ptr(), ok: &mut ok };
            // SAFETY: the frame is the ctx; every pointer is live for the call.
            let r = unsafe { code.call((&mut frame as *mut LaneFrame).cast()) };
            assert_eq!(r, 0);
            assert_eq!(ok, expect_ok as u64, "{name} {arm}: the fits guard");
            if expect_ok {
                let masked: Vec<u64> = out_vec.iter().enumerate().map(|(w, m)| m & av[w] & bv[w]).collect();
                assert_eq!(masked, out_row, "{name} {arm}: the vector word under the validity words equals the scalar rows");
            }
            // the scalar rows against eval, validity included (a null row never sets its bit)
            let mut buf = vec![0u8; 4096];
            let mut region = Region::over(&mut buf);
            let mut c2 = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
            for i in 0..ROWS {
                let r = Row { scan: gravity_expr::Cols::Lanes { lanes: &lanes, index: i }, inner: gravity_expr::Cols::None, outer: gravity_expr::Cols::None, aggs: &[] };
                let v = c2.eval(&r, &mut region).unwrap();
                let hit = !v.isnull && v.value.as_bool();
                assert_eq!((out_row[i / 64] >> (i % 64)) & 1 != 0, hit, "{name} {arm}: row {i}");
            }
        }
    }
}

#[test]
fn an_or_across_lanes_is_not_a_vector_word() {
    install_seams();
    let cx = MemoryContext::new("emit-word-or");
    let mcx = cx.mcx();
    let e = boolexpr(mcx, BoolExprType::OR_EXPR, &[opexpr(mcx, 65, BOOLOID, 0, &[scan_var(mcx, 1, INT4OID), int4(mcx, Some(7))]), opexpr(mcx, 467, BOOLOID, 0, &[scan_var(mcx, 2, INT8OID), int8(mcx, Some(-3))])]);
    let c = Expr::compile(lists(), e, &PgCatalog, ParamBind::NONE).unwrap();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("w", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let zero = b.iconst(Type::I64, 0);
    let fl = b.bconst(false);
    let lanes = [Some(LaneRef { ptr: ctx, valid: ctx, valid_null: None, width: 4, kind: 0, all_valid: true }), Some(LaneRef { ptr: ctx, valid: ctx, valid_null: None, width: 8, kind: 0, all_valid: true })];
    let w = WordRefs { scratch: scratch(), lanes: &lanes, consts: &[zero, zero], consts_null: &[fl, fl], tables: &[] };
    assert_eq!(c.emit_word(&mut b, &w, zero).err(), Some(EmitError::NotVector));
    assert_eq!(c.emit_word_ok(&mut b, &w).err(), Some(EmitError::NotVector));
}

#[test]
fn refusals_name_the_kind() {
    install_seams();
    let cx = MemoryContext::new("emit-refusal");
    let mcx = cx.mcx();
    let c = Expr::compile(lists(), case_test(mcx), &PgCatalog, ParamBind::NONE).unwrap();
    assert_eq!(build(&c, 1, 0, false, false).err(), Some(EmitError::Unsupported(gravity_expr::Kind::CaseTestExpr)));
    let c = Expr::compile(lists(), Node::mk(mcx, types_nodes::primnodes::SQLValueFunction { op: types_nodes::primnodes::SQLValueFunctionOp::SVFOP_CURRENT_DATE, r#type: DATEOID, typmod: -1, location: -1 }).unwrap(), &PgCatalog, ParamBind::NONE).unwrap();
    // CURRENT_DATE is `now()` cast to date: two host sites, served
    assert_eq!(c.emit_check(), Ok(()));
    assert_eq!(c.host_calls(), 2);
    let c = Expr::compile(lists(), funcexpr(mcx, 42, INT4OID, 0, &[cstring_const(mcx, "1")]), &PgCatalog, ParamBind::NONE).unwrap();
    assert_eq!(c.host_calls(), 1);
    let c = Expr::compile(lists(), opexpr(mcx, 177, INT4OID, 0, &[int4(mcx, Some(1)), int4(mcx, Some(2))]), &PgCatalog, ParamBind::NONE).unwrap();
    assert_eq!(c.host_calls(), 0);
    let _ = Value(0);
    let _: fn(&mut FunctionBuilder<'_>) = |_| {};
}

// ---- jsonb: the shredded lane form against the value form (expr-compiler.md §2.7) ----

fn jsonb_image<'m>(mcx: Mcx<'m>, json: &str) -> u64 {
    let img = adt_jsonb::io::jsonb_in(mcx, json.as_bytes(), None).unwrap().expect("hard path");
    let d = img.as_ptr() as usize as u64;
    core::mem::forget(img);
    d
}

fn coerce_via_io<'a>(mcx: Mcx<'a>, arg: Node<'a>, typ: Oid) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::CoerceViaIO { arg, resulttype: typ, resultcollid: 0, coerceformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap()
}

/// The lanes frame: lane 1 the jsonb column, 2 / 3 the text leaves `kind` and `commit.collection`
/// (by-reference datum pointers), 4 the fixed int8 leaf `time_us`; validity = the key present.
#[repr(C)]
struct ShredFrame {
    lanes: [gravity_abi::ctx::Lane; 4],
    rows: u64,
    consts: *const u64,
    consts_null: *const u8,
    region: *mut Region,
    cells: *mut gravity_expr::emit::Cell,
    out_val: *mut u64,
    out_valid: *mut u8,
    out_err: *mut u64,
}

fn shred_program(c: &Compiled<'_>, nconsts: usize, shredded: bool) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    use gravity_abi::ctx::lane;
    use gravity_expr::emit::jsonb::{ShredRef, ShredType};
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("shred_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let after = (4 * lane::SIZE) as i32;
    let rows = b.load(Type::I64, ctx, after);
    let cp = b.load(Type::Ptr, ctx, after + 8);
    let cnp = b.load(Type::Ptr, ctx, after + 16);
    let region = b.load(Type::Ptr, ctx, after + 24);
    let cells = b.load(Type::Ptr, ctx, after + 32);
    let out_val = b.load(Type::Ptr, ctx, after + 40);
    let out_valid = b.load(Type::Ptr, ctx, after + 48);
    let out_err = b.load(Type::Ptr, ctx, after + 56);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let mut consts = Vec::new();
    let mut consts_null = Vec::new();
    for j in 0..nconsts {
        consts.push(b.load(Type::I64, cp, (j * 8) as i32));
        let n = b.load_zext(Type::I8, cnp, j as i32);
        consts_null.push(b.icmp(IntCond::Ne, n, zero));
    }
    let lane_ref = |b: &mut FunctionBuilder<'_>, l: usize, kind: u8| {
        let off = (l * lane::SIZE) as i32;
        let ptr = b.load(Type::Ptr, ctx, off + lane::PTR as i32);
        let valid = b.load(Type::Ptr, ctx, off + lane::VALID as i32);
        LaneRef { ptr, valid, valid_null: None, width: 8, kind, all_valid: false }
    };
    let data = lane_ref(&mut b, 0, lane::KIND_BYREF);
    let lanes = [Some(data)];
    let shreds = if shredded {
        vec![
            ShredRef { attno: 1, path: vec![b"kind".to_vec()], lane: lane_ref(&mut b, 1, lane::KIND_BYREF), typ: ShredType::Text },
            ShredRef { attno: 1, path: vec![b"commit".to_vec(), b"collection".to_vec()], lane: lane_ref(&mut b, 2, lane::KIND_BYREF), typ: ShredType::Text },
            ShredRef { attno: 1, path: vec![b"time_us".to_vec()], lane: lane_ref(&mut b, 3, lane::KIND_BYVAL), typ: ShredType::Fixed(INT8OID) },
        ]
    } else {
        Vec::new()
    };
    let l_head = b.create_block(&[Type::I64]);
    let l_row = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let i = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, i, rows);
    b.cond_br(done, (l_out, &[]), (l_row, &[]));
    b.switch_to(l_row);
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: i, lanes: &lanes }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: Some(region), cells: Some(cells), shreds: &shreds, codes: &[], tables: &[], rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = c.emit(&mut b, &row)?;
    let ov = b.ptr_add(out_val, i, 8, 0);
    b.store(ov, 0, l.value);
    let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let ovp = b.ptr_add(out_valid, i, 1, 0);
    b.store(ovp, 0, valid8);
    let oep = b.ptr_add(out_err, i, 8, 0);
    b.store(oep, 0, l.err);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_head, &[i1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

#[test]
fn jsonb_shredded_lanes_match_the_value_form_eval_and_the_incumbent() {
    install_seams();
    let cx = MemoryContext::new("emit-jsonb");
    let mcx = cx.mcx();
    let docs: [Option<&str>; 6] = [
        Some(r#"{"kind":"commit","commit":{"collection":"app.bsky.feed.post","operation":"create"},"did":"u1","time_us":1700000000123456}"#),
        Some(r#"{"kind":"commit","commit":{"collection":"app.bsky.feed.like","operation":"create"},"did":"u2","time_us":1700000000223456}"#),
        Some(r#"{"kind":"identity","did":"u3","time_us":1700000000323456}"#),
        Some(r#"{"kind":"commit","commit":{"operation":"delete"},"did":"u4"}"#),
        None,
        Some(r#"{"kind":"commit","commit":{"collection":"app.bsky.feed.post","operation":"create"},"did":"u5","time_us":1700000000523456}"#),
    ];
    const ROWS: usize = 6;
    let data: Vec<u64> = docs.iter().map(|d| d.map_or(0, |j| jsonb_image(mcx, j))).collect();
    let data_v = [docs.iter().enumerate().fold(0u64, |m, (i, d)| m | ((d.is_some() as u64) << i))];
    let kind: Vec<u64> = docs.iter().map(|d| d.map_or(0, |j| if j.contains("\"kind\":\"commit\"") { text_image(mcx, "commit").as_u64() } else { text_image(mcx, "identity").as_u64() })).collect();
    let kind_v = data_v;
    let coll: Vec<u64> = docs.iter().map(|d| d.map_or(0, |j| if j.contains("feed.post") { text_image(mcx, "app.bsky.feed.post").as_u64() } else if j.contains("feed.like") { text_image(mcx, "app.bsky.feed.like").as_u64() } else { 0 })).collect();
    let coll_v = [docs.iter().enumerate().fold(0u64, |m, (i, d)| m | ((d.is_some_and(|j| j.contains("collection")) as u64) << i))];
    let tus: Vec<u64> = [1700000000123456u64, 1700000000223456, 1700000000323456, 0, 0, 1700000000523456].to_vec();
    let tus_v = [0b100111u64];
    let lanes = [
        gravity_abi::ctx::Lane { ptr: data.as_ptr().cast(), valid: data_v.as_ptr(), width: 8, kind: gravity_abi::ctx::lane::KIND_BYREF, region: 0, flags: 0, ncodes_or_scale: 0, id: 0, aux: 0 },
        gravity_abi::ctx::Lane { ptr: kind.as_ptr().cast(), valid: kind_v.as_ptr(), width: 8, kind: gravity_abi::ctx::lane::KIND_BYREF, region: 0, flags: 0, ncodes_or_scale: 0, id: 0, aux: 0 },
        gravity_abi::ctx::Lane { ptr: coll.as_ptr().cast(), valid: coll_v.as_ptr(), width: 8, kind: gravity_abi::ctx::lane::KIND_BYREF, region: 0, flags: 0, ncodes_or_scale: 0, id: 0, aux: 0 },
        gravity_abi::ctx::Lane::byval(tus.as_ptr().cast(), tus_v.as_ptr(), 8, 0, 0),
    ];
    let exprs: Vec<(&str, Oid, Box<dyn Fn(Mcx<'_>) -> Node<'_>>)> = vec![
        ("data->>'kind'", TEXTOID, Box::new(|m| opexpr(m, 3214, TEXTOID, 0, &[scan_var(m, 1, JSONBOID), text_const(m, "kind")]))),
        ("data->'commit'->>'collection'", TEXTOID, Box::new(|m| opexpr(m, 3214, TEXTOID, 0, &[opexpr(m, 3478, JSONBOID, 0, &[scan_var(m, 1, JSONBOID), text_const(m, "commit")]), text_const(m, "collection")]))),
        ("data->>'kind' = 'commit'", BOOLOID, Box::new(|m| opexpr(m, 67, BOOLOID, 950, &[opexpr(m, 3214, TEXTOID, 0, &[scan_var(m, 1, JSONBOID), text_const(m, "kind")]), text_const(m, "commit")]))),
        ("(data->>'time_us')::bigint", INT8OID, Box::new(|m| coerce_via_io(m, opexpr(m, 3214, TEXTOID, 0, &[scan_var(m, 1, JSONBOID), text_const(m, "time_us")]), INT8OID))),
        ("data->'commit'->>'operation' (no lane: the value form)", TEXTOID, Box::new(|m| opexpr(m, 3214, TEXTOID, 0, &[opexpr(m, 3478, JSONBOID, 0, &[scan_var(m, 1, JSONBOID), text_const(m, "commit")]), text_const(m, "operation")]))),
    ];
    let mut total = 0;
    let mut bad = Vec::new();
    for (name, typ, build) in exprs {
        let expr = build(mcx);
        let types = [JSONBOID];
        let mut incumbent = Vec::new();
        for i in 0..ROWS {
            let mut state = exec_init_expr(mcx, Some(expr), ParamBind::NONE).unwrap().unwrap();
            state.arm_result_mcx(mcx);
            let row = [docs[i].map(|_| data[i])];
            let mut slot = virtual_slot(mcx, &types, &row);
            let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
            incumbent.push(match exec_eval_expr(&mut state, &mut slots) { Ok(v) => bytes_of(pg_value(v, typ), typ), Err(e) => err_of(&e) });
        }
        let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut buf = vec![0u8; 1 << 16];
        let mut region = Region::over(&mut buf);
        let mut eval = Vec::new();
        for i in 0..ROWS {
            let r = Row { scan: gravity_expr::Cols::Lanes { lanes: &lanes[..1], index: i }, inner: gravity_expr::Cols::None, outer: gravity_expr::Cols::None, aggs: &[] };
            eval.push(match compiled.eval(&r, &mut region) { Ok(v) => bytes_of(v, typ), Err(e) => err_of(&e) });
        }
        let needs = compiled.param_needs(scratch()).unwrap();
        let consts: Vec<u64> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.value.as_u64(), _ => 0 }).collect();
        let consts_null: Vec<u8> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.isnull as u8, _ => 1 }).collect();
        let mut state: EmitState = compiled.emit_state(emit_cx());
        for shredded in [false, true] {
            let (ir, f) = shred_program(&compiled, needs.len(), shredded).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let hosts = ir.host_fns.len();
            for (arm, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{name}: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
                let mut out_val = vec![0u64; ROWS];
                let mut out_valid = vec![0u8; ROWS];
                let mut out_err = vec![0u64; ROWS];
                let mut rbuf = vec![0u8; 1 << 16];
                let mut region = Region::over(&mut rbuf);
                let mut frame = ShredFrame { lanes, rows: ROWS as u64, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), region: &mut region, cells: state.ptr(), out_val: out_val.as_mut_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr() };
                // SAFETY: the frame is the ctx; every pointer is live for the call.
                let r = unsafe { code.call((&mut frame as *mut ShredFrame).cast()) };
                assert_eq!(r, 0);
                for i in 0..ROWS {
                    total += 1;
                    let got = if out_err[i] != 0 { raised(out_err[i]) } else if out_valid[i] == 0 { Outcome::Null } else { bytes_of(NullableDatum { value: Datum::from_u64(out_val[i]), isnull: false }, typ) };
                    if got != incumbent[i] || eval[i] != incumbent[i] {
                        bad.push(format!("{name} shredded={shredded} {arm} row {i}: got {got:?} eval {:?} incumbent {:?}", eval[i], incumbent[i]));
                    }
                }
            }
            if shredded {
                // the chain is a lane load; `= 'commit'` keeps its texteq shim; the unshredded path its shims
                let want = if name.contains("no lane") || name.contains("= 'commit'") { 1 } else { 0 };
                assert_eq!(hosts, want, "{name}: the shredded form's host declarations");
            }
        }
    }
    eprintln!("jsonb shredded conformance: {total} rows compared");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

// ---- per-code evaluation (expr-compiler.md §7): the analysis, the fill, the row form ----

use gravity_expr::emit::{CodeRef, TableRef};
use gravity_expr::percode::{fill, CodeCol, ConstWord, Form, Known};

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

/// NEW-72 entry C: the text a bound table's constant word holds (`Known::EqBound`'s consts, `Known::RangeBound`'s `at`):
/// the Const's image, or element `elem` of the array Const, read here as the source reads it per execution.
fn const_text(c: &Compiled<'_>, w: ConstWord) -> Vec<u8> {
    let gravity_expr::nodes::ExprNode::Const(k) = c.node(w.node) else { panic!("{w:?}: not a Const") };
    let d = k.read();
    assert!(!d.isnull, "{w:?}: a NULL constant");
    let mut small = [0u8; 256];
    let v = match w.elem {
        None => d.value,
        Some(i) => {
            let elem = gravity_expr::nodes::array::ElemFacts { typ: TEXTOID, len: -1, byval: false, align: b'i' as i8 };
            let mut walk = gravity_expr::nodes::array::Walk::over(d.value, elem, &mut gravity_fn::Region::over(&mut small)).unwrap();
            let e = (0..=i).map(|_| walk.next().unwrap()).last().unwrap();
            assert!(!e.isnull, "{w:?}: a NULL element");
            e.value
        }
    };
    gravity_expr::views::payload_view(v, -1, &mut gravity_fn::Region::over(&mut small)).unwrap().bytes().to_vec()
}

fn kl(p: &[u8], ilike: bool, negated: bool) -> Known<'static> {
    Known::Like { pattern: mcx::to_vec_in(p, emit_cx()), ilike, negated }
}

fn keq(vs: &[&[u8]], negated: bool) -> Known<'static> {
    let m = emit_cx();
    Known::Eq { values: mcx::CollectIn::collect_in(vs.iter().map(|v| mcx::to_vec_in(v, m)), m), negated }
}

fn krange(from: &[u8], to: Option<&[u8]>) -> Known<'static> {
    let m = emit_cx();
    Known::Range { from: mcx::to_vec_in(from, m), to: to.map(|t| mcx::to_vec_in(t, m)) }
}

/// A copy of a `Known` (the library's is not `Clone`: the lowering moves it).
fn dup<'a>(k: &Known<'a>) -> Known<'a> {
    match k {
        Known::Like { pattern, ilike, negated } => Known::Like { pattern: pattern.clone(), ilike: *ilike, negated: *negated },
        Known::Eq { values, negated } => Known::Eq { values: values.clone(), negated: *negated },
        Known::EqBound { values, params, consts, negated } => Known::EqBound { values: values.clone(), params: params.clone(), consts: consts.clone(), negated: *negated },
        Known::LikeBound { parts, params, ilike, negated } => Known::LikeBound { parts: parts.clone(), params: params.clone(), ilike: *ilike, negated: *negated },
        Known::Range { from, to } => Known::Range { from: from.clone(), to: to.clone() },
        Known::RangeBound { at, below, nul } => Known::RangeBound { at: *at, below: *below, nul: *nul },
        Known::OctetLength => Known::OctetLength,
        Known::CharLength => Known::CharLength,
        Known::Body => Known::Body,
    }
}

/// NEW-72 entry C: a bound constant table as the baked shape its constants make (`Known::Eq` / `Known::Range`), so the
/// cases state the set or the interval the source builds; any other need as it is.
fn bound_as_baked<'a>(c: &Compiled<'_>, k: &Known<'a>) -> Known<'a> {
    match k {
        Known::EqBound { values, params, consts, negated } if values.is_empty() && params.is_empty() => keq(&consts.iter().map(|&w| const_text(c, w)).collect::<Vec<_>>().iter().map(|v| v.as_slice()).collect::<Vec<_>>(), *negated),
        Known::RangeBound { at, below, nul } => {
            let mut v = const_text(c, *at);
            if *nul {
                v.push(0);
            }
            if *below { krange(b"", Some(&v)) } else { krange(&v, None) }
        }
        k => dup(k),
    }
}

const F_TEXTLIKE: Oid = 850;
const F_TEXTNLIKE: Oid = 851;
const F_TEXTNE: Oid = 157;
const F_TEXTEQ: Oid = 67;
/// text_lt; + 1 text_le, + 2 text_gt, + 3 text_ge (pg_proc.dat).
const F_TEXT_LT: Oid = 740;
/// text_pattern_lt; + 1 _le, + 3 _ge, + 4 _gt (pg_proc.dat).
const F_TEXT_PATTERN_LT: Oid = 2160;
const F_OCTET_LENGTH: Oid = 1374;
const F_UPPER: Oid = 871;

struct VecDict(Vec<Vec<u8>>);

impl gravity_dict::Dict for VecDict {
    fn ncodes(&self) -> u32 {
        self.0.len() as u32
    }
    fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
        for (i, e) in self.0.iter().enumerate() {
            f(i as u32, gravity_dict::View::of(e));
        }
    }
}

fn percode_dict() -> VecDict {
    VecDict(["", "http://garden.com/", "http://search.ru/", "http://mail.garden.com/x", "plain", "Foo", "foo bar", "пример"].iter().map(|s| s.as_bytes().to_vec()).collect())
}

/// 90 rows: code `i % 8`, NULL every 11th row (a validity word per 64 rows).
fn percode_rows() -> (Vec<u32>, Vec<u64>) {
    let codes: Vec<u32> = (0..90u32).map(|i| i % 8).collect();
    let mut valid = vec![0u64; 2];
    for i in 0..90usize {
        if i % 11 != 0 {
            valid[i / 64] |= 1 << (i % 64);
        }
    }
    (codes, valid)
}

/// The synthetic dictionary index (12-byte entries: offset, byte length, char field) of `dict`.
fn percode_index(dict: &VecDict) -> Vec<u8> {
    let mut out = Vec::new();
    let mut off = 0u32;
    for e in &dict.0 {
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&(e.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        off += e.len() as u32;
    }
    out
}

#[repr(C)]
struct PcFrame {
    codes: *const u32,
    valid: *const u64,
    nrows: u64,
    out_val: *mut u64,
    out_valid: *mut u8,
    out_err: *mut u64,
    tables: [*const u8; 4],
}

#[derive(Clone, Copy)]
enum PcForm {
    Bits,
    Words,
    IdxLen,
    /// `TableRef::Len` with the form word from the frame's table slot after the table (`form` is per window in production).
    Len(u8),
}

/// The row loop over a u32 code lane with a validity word: `emit` per row over the declared code
/// lane and the bound tables.
/// `pred`: the truth form (§7.7) — `emit_pred`'s `cond` selected to 1 / 0 as the row's value.
fn percode_build(c: &Compiled<'_>, forms: &[PcForm], pred: bool) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    use gravity_abi::ctx::lane;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("percode_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let ptr = b.load(Type::Ptr, ctx, 0);
    let valid = b.load(Type::Ptr, ctx, 8);
    let nrows = b.load(Type::I64, ctx, 16);
    let out_val = b.load(Type::Ptr, ctx, 24);
    let out_valid = b.load(Type::Ptr, ctx, 32);
    let out_err = b.load(Type::Ptr, ctx, 40);
    let mut tables = Vec::new();
    for (i, form) in forms.iter().enumerate() {
        let p = b.load(Type::Ptr, ctx, 48 + 8 * i as i32);
        tables.push(match form {
            PcForm::Bits => TableRef::Bits { words: p, faults: None },
            PcForm::Words => TableRef::Words { vals: p, faults: None },
            PcForm::IdxLen => TableRef::IdxLen { index: p, field: gravity_dict::LenField::Bytes },
            PcForm::Len(f) => {
                let form = b.iconst(Type::I64, *f as i64);
                gravity_expr::emit::percode::len_table(&mut b, p, form)
            }
        });
    }
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let l_head = b.create_block(&[Type::I64]);
    let l_row = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let i = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, i, nrows);
    b.cond_br(done, (l_out, &[]), (l_row, &[]));
    b.switch_to(l_row);
    let lane = LaneRef { ptr, valid, valid_null: None, width: 4, kind: lane::KIND_CODES, all_valid: false };
    let codes = [CodeRef { attno: 1, lane, code: None, ordinal: 0, part: None }];
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: i, lanes: &[] }, inner: ColsRef::None, outer: ColsRef::None, consts: &[], consts_null: &[], region: None, cells: None, shreds: &[], codes: &codes, tables: &tables, rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = if pred {
        let p = c.emit_pred(&mut b, &row)?;
        let value = b.select(p.cond, one, zero);
        gravity_expr::emit::Lowered { value, valid: p.valid, err: p.err }
    } else {
        c.emit(&mut b, &row)?
    };
    let ov = b.ptr_add(out_val, i, 8, 0);
    b.store(ov, 0, l.value);
    let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let ovp = b.ptr_add(out_valid, i, 1, 0);
    b.store(ovp, 0, valid8);
    let oep = b.ptr_add(out_err, i, 8, 0);
    b.store(oep, 0, l.err);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_head, &[i1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

struct PcCase {
    name: &'static str,
    typ: Oid,
    forms: Vec<Form>,
    known: Vec<Known<'static>>,
    /// The tables' forms when bound: the fill's, or the R4 index view for a length.
    idx: bool,
    build: Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>,
}

fn pc(name: &'static str, typ: Oid, forms: &[Form], known: &[Known<'static>], idx: bool, build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> PcCase {
    PcCase { name, typ, forms: forms.to_vec(), known: known.iter().map(dup).collect(), idx, build: Box::new(build) }
}

fn like<'m>(m: Mcx<'m>, f: Oid, pat: &str) -> Node<'m> {
    opexpr(m, f, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, pat)])
}

fn pc_cases() -> Vec<PcCase> {
    vec![
        pc("like", BOOLOID, &[Form::Bits], &[kl(b"%garden%", false, false)], false, move |m| like(m, F_TEXTLIKE, "%garden%")),
        pc("not like", BOOLOID, &[Form::Bits], &[kl(b"%garden%", false, true)], false, move |m| like(m, F_TEXTNLIKE, "%garden%")),
        pc("NOT (like)", BOOLOID, &[Form::Bits], &[kl(b"http://%", false, true)], false, move |m| boolexpr(m, BoolExprType::NOT_EXPR, &[like(m, F_TEXTLIKE, "http://%")])),
        pc("= '' as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b""], false)], false, |m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "")])),
        pc("NOT (= '') as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b""], true)], false, |m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "")])])),
        pc("<> '' as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b""], true)], false, |m| opexpr(m, F_TEXTNE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "")])),
        pc("= 'plain' as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b"plain"], false)], false, |m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "plain")])),
        pc("'plain' = x, the constant first (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b"plain"], false)], false, |m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[text_const(m, "plain"), scan_var(m, 1, TEXTOID)])),
        pc("NOT (= 'plain') (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b"plain"], true)], false, |m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "plain")])])),
        pc("IN (...) as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b"plain", b"absent", b"", "пример".as_bytes()], false)], false, |m| saop_by(m, 98, F_TEXTEQ, 950, true, scan_var(m, 1, TEXTOID), text_array_const(m, &["plain", "absent", "", "пример"]))),
        pc("NOT IN (...) as a code set (B2-20)", BOOLOID, &[Form::Bits], &[keq(&[b"Foo", b"foo bar"], true)], false, |m| saop_by(m, 531, F_TEXTNE, 950, false, scan_var(m, 1, TEXTOID), text_array_const(m, &["Foo", "foo bar"]))),
        pc("= ALL (...) is no code set (B2-20)", BOOLOID, &[Form::Bits], &[Known::Body], false, |m| saop_by(m, 98, F_TEXTEQ, 950, false, scan_var(m, 1, TEXTOID), text_array_const(m, &["plain", "plain"]))),
        pc("<> ANY (...) is no code set (B2-20)", BOOLOID, &[Form::Bits], &[Known::Body], false, |m| saop_by(m, 531, F_TEXTNE, 950, true, scan_var(m, 1, TEXTOID), text_array_const(m, &["plain", "plain"]))),
        pc("< 'm' as a code range (P5-06)", BOOLOID, &[Form::Bits], &[krange(b"", Some(b"m"))], false, |m| opexpr(m, F_TEXT_LT, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "m")])),
        pc("<= 'm' as a code range (P5-06)", BOOLOID, &[Form::Bits], &[krange(b"", Some(b"m\0"))], false, |m| opexpr(m, F_TEXT_LT + 1, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "m")])),
        pc("'m' < x, the constant first (P5-06)", BOOLOID, &[Form::Bits], &[krange(b"m\0", None)], false, |m| opexpr(m, F_TEXT_LT, BOOLOID, 950, &[text_const(m, "m"), scan_var(m, 1, TEXTOID)])),
        pc(">= 'пример' under POSIX (P5-06)", BOOLOID, &[Form::Bits], &[krange("пример".as_bytes(), None)], false, |m| opexpr(m, F_TEXT_LT + 3, BOOLOID, 951, &[scan_var(m, 1, TEXTOID), text_const(m, "пример")])),
        pc("NOT (> 'm') (P5-06)", BOOLOID, &[Form::Bits], &[krange(b"", Some(b"m\0"))], false, |m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, F_TEXT_LT + 2, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "m")])])),
        pc("~>=~ 'foo' under the database default (P5-06: the pattern family compares bytes)", BOOLOID, &[Form::Bits], &[krange(b"foo", None)], false, |m| opexpr(m, F_TEXT_PATTERN_LT + 3, BOOLOID, 100, &[scan_var(m, 1, TEXTOID), text_const(m, "foo")])),
        pc("octet_length (words)", INT4OID, &[Form::Words], &[Known::OctetLength], false, |m| funcexpr(m, F_OCTET_LENGTH, INT4OID, 0, &[scan_var(m, 1, TEXTOID)])),
        pc("octet_length (index view)", INT4OID, &[Form::Words], &[Known::OctetLength], true, |m| funcexpr(m, F_OCTET_LENGTH, INT4OID, 0, &[scan_var(m, 1, TEXTOID)])),
        pc("upper(x) = 'FOO'", BOOLOID, &[Form::Bits], &[Known::Body], false, |m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[funcexpr(m, F_UPPER, TEXTOID, 950, &[scan_var(m, 1, TEXTOID)]), text_const(m, "FOO")])),
        pc("octet_length(x) > 4 (a wider body)", BOOLOID, &[Form::Bits], &[Known::Body], false, |m| opexpr(m, 147, BOOLOID, 0, &[funcexpr(m, F_OCTET_LENGTH, INT4OID, 0, &[scan_var(m, 1, TEXTOID)]), int4(m, Some(4))])),
        pc("the text key (code word)", TEXTOID, &[Form::Code], &[], false, |m| scan_var(m, 1, TEXTOID)),
        pc("like AND <> '' (two roots)", BOOLOID, &[Form::Bits, Form::Bits], &[kl(b"%o%", false, false), keq(&[b""], true)], false, move |m| boolexpr(m, BoolExprType::AND_EXPR, &[like(m, F_TEXTLIKE, "%o%"), opexpr(m, F_TEXTNE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), text_const(m, "")])])),
    ]
}

#[repr(C)]
struct PcWordFrame {
    codes: *const u32,
    nwords: u64,
    out_vec: *mut u64,
    out_row: *mut u64,
    tables: [*const u8; 4],
}

/// The word form over a code lane (§7 under `emit_word_codes`): per 64-row word the mask, and per
/// row the truth form's bit, both stored.
fn percode_word_build(c: &Compiled<'_>, forms: &[PcForm]) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    use gravity_abi::ctx::lane;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("percode_word_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let ptr = b.load(Type::Ptr, ctx, 0);
    let nwords = b.load(Type::I64, ctx, 8);
    let out_vec = b.load(Type::Ptr, ctx, 16);
    let out_row = b.load(Type::Ptr, ctx, 24);
    let mut tables = Vec::new();
    for (i, form) in forms.iter().enumerate() {
        let p = b.load(Type::Ptr, ctx, 32 + 8 * i as i32);
        tables.push(match form {
            PcForm::Bits => TableRef::Bits { words: p, faults: None },
            PcForm::Words => TableRef::Words { vals: p, faults: None },
            PcForm::IdxLen => TableRef::IdxLen { index: p, field: gravity_dict::LenField::Bytes },
            PcForm::Len(f) => {
                let form = b.iconst(Type::I64, *f as i64);
                gravity_expr::emit::percode::len_table(&mut b, p, form)
            }
        });
    }
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let six = b.iconst(Type::I64, 6);
    let lane = LaneRef { ptr, valid: ptr, valid_null: None, width: 4, kind: lane::KIND_CODES, all_valid: true };
    let codes = [CodeRef { attno: 1, lane, code: None, ordinal: 0, part: None }];
    let w = WordRefs { scratch: scratch(), lanes: &[], consts: &[], consts_null: &[], tables: &tables };
    let l_head = b.create_block(&[Type::I64]);
    let l_word = b.create_block(&[]);
    let l_rows = b.create_block(&[Type::I64, Type::I64]);
    let l_row = b.create_block(&[]);
    let l_store = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.mark_loop_header(l_rows);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let wi = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, wi, nwords);
    b.cond_br(done, (l_out, &[]), (l_word, &[]));
    b.switch_to(l_word);
    let base = b.int_bin(IntBinOp::Shl, wi, six);
    let m = c.emit_word_codes(&mut b, &w, &codes, base)?;
    let mp = b.ptr_add(out_vec, wi, 8, 0);
    b.store(mp, 0, m);
    b.br(l_rows, &[zero, zero]);
    b.switch_to(l_rows);
    let (i, acc) = (b.block_params(l_rows)[0], b.block_params(l_rows)[1]);
    let m64 = b.iconst(Type::I64, 64);
    let rdone = b.icmp(IntCond::Uge, i, m64);
    b.cond_br(rdone, (l_store, &[]), (l_row, &[]));
    b.switch_to(l_store);
    let rp = b.ptr_add(out_row, wi, 8, 0);
    b.store(rp, 0, acc);
    let wi1 = b.int_bin(IntBinOp::Add, wi, one);
    b.br(l_head, &[wi1]);
    b.switch_to(l_row);
    let ri = b.int_bin(IntBinOp::Add, base, i);
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: ri, lanes: &[] }, inner: ColsRef::None, outer: ColsRef::None, consts: &[], consts_null: &[], region: None, cells: None, shreds: &[], codes: &codes, tables: &tables, rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let p = c.emit_pred(&mut b, &row)?;
    let hit = b.int_bin(IntBinOp::And, p.valid, p.cond);
    let bit = b.conv(ConvOp::Zext, Type::I64, hit);
    let sh = b.int_bin(IntBinOp::Shl, bit, i);
    let acc2 = b.int_bin(IntBinOp::Or, acc, sh);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_rows, &[i1, acc2]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

/// The LIKE bit (and every other `Bits` root) as a word (§7, lower.md §6.11 need 10): over 256
/// all-valid rows of a code lane, `emit_word_codes`'s mask per 64-row word equals the truth form's
/// bits per row, interp = TPDE; a `Words` root has no word form.
#[test]
fn percode_bits_word_matches_the_scalar_rows() {
    install_seams();
    let cx = MemoryContext::new("percode-word-test");
    let mcx = cx.mcx();
    let dict = percode_dict();
    let codes: Vec<u32> = (0..256u32).map(|i| (i * 5 + i / 7) % 8).collect();
    let mut n = 0;
    for c in pc_cases().into_iter().filter(|c| c.typ == BOOLOID && !c.idx) {
        let expr = (c.build)(mcx);
        let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
        let cols = [CodeCol { attno: 1 }];
        let plan = compiled.percode(&cols, scratch()).unwrap();
        let mut bufs: Vec<Vec<u64>> = Vec::new();
        let mut forms = Vec::new();
        let mut ptrs = [core::ptr::null::<u8>(); 4];
        for (i, need) in plan.needs.iter().enumerate() {
            let mut buf = vec![0u64; 1 << 12];
            let mut region = unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
            let mut scratch = vec![0u8; 1 << 12];
            let hdr = fill(&mut compiled, need, &dict, &mut region, &mut scratch).unwrap();
            ptrs[i] = unsafe { (*hdr).data };
            forms.push(if need.elem == gravity_dict::ELEM_BITS { PcForm::Bits } else { PcForm::Words });
            bufs.push(buf);
        }
        let word_form = plan.roots.iter().all(|r| matches!(r.form, Form::Bits));
        let built = percode_word_build(&compiled, &forms);
        if !word_form {
            assert_eq!(built.err(), Some(EmitError::NotVector), "{}: no word form", c.name);
            continue;
        }
        let (ir, f) = built.unwrap_or_else(|e| panic!("{}: {e:?}", c.name));
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", c.name));
        for (arm, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: {}", c.name, e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
            let mut out_vec = vec![0u64; 4];
            let mut out_row = vec![0u64; 4];
            let mut frame = PcWordFrame { codes: codes.as_ptr(), nwords: 4, out_vec: out_vec.as_mut_ptr(), out_row: out_row.as_mut_ptr(), tables: ptrs };
            // SAFETY: the frame is the ctx; every pointer is live for the call.
            let r = unsafe { code.call((&mut frame as *mut PcWordFrame).cast()) };
            assert_eq!(r, 0);
            assert_eq!(out_vec, out_row, "{} {arm}: the word equals the rows", c.name);
            assert!(out_vec.iter().any(|m| *m != 0 && *m != !0), "{} {arm}: a mixed word", c.name);
        }
        n += 1;
    }
    assert!(n >= 5, "{n} word-form cases");
}

/// `TableRef::Len` (lowerer-ladder §3 rung 4): one root over whichever length table the window's part publishes,
/// the form a word — 0 an `I64` table (`octet_length`'s fill), 1 the index's byte length, 3 its char field, 2 the
/// char length from a `bytes − chars` delta field — interp = TPDE = the entry's byte / char count per row, NULLs kept.
#[test]
fn percode_len_reads_every_length_form() {
    install_seams();
    let cx = MemoryContext::new("percode-len-test");
    let mcx = cx.mcx();
    let dict = percode_dict();
    let (codes, valid) = percode_rows();
    let expr = funcexpr(mcx, F_OCTET_LENGTH, INT4OID, 0, &[scan_var(mcx, 1, TEXTOID)]);
    let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
    let cols = [CodeCol { attno: 1 }];
    let plan = compiled.percode(&cols, scratch()).unwrap();
    assert_eq!(plan.needs.iter().map(|n| dup(&n.known)).collect::<Vec<_>>(), vec![Known::OctetLength]);
    // the I64 table: the fill; the index views: (offset, bytes, chars) and (offset, bytes, bytes − chars)
    let mut buf = vec![0u64; 1 << 12];
    let mut region = unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
    let mut scratch = vec![0u8; 1 << 12];
    let words = unsafe { (*fill(&mut compiled, &plan.needs[0], &dict, &mut region, &mut scratch).unwrap()).data };
    let chars_of = |e: &[u8]| core::str::from_utf8(e).unwrap().chars().count() as u32;
    let index = |third: &dyn Fn(&[u8]) -> u32| -> Vec<u8> {
        let mut out = Vec::new();
        let mut off = 0u32;
        for e in &dict.0 {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&(e.len() as u32).to_le_bytes());
            out.extend_from_slice(&third(e).to_le_bytes());
            off += e.len() as u32;
        }
        out
    };
    let absolute = index(&chars_of);
    let delta = index(&|e| e.len() as u32 - chars_of(e));
    assert!(dict.0.iter().any(|e| e.len() as u32 != chars_of(e)), "a multibyte entry tells the forms apart");
    let nrows = codes.len();
    for (form, base, chars) in [(0u8, words, false), (1, unsafe { absolute.as_ptr().add(4) }, false), (3, unsafe { absolute.as_ptr().add(4) }, true), (2, unsafe { delta.as_ptr().add(4) }, true)] {
        let (ir, f) = percode_build(&compiled, &[PcForm::Len(form)], false).unwrap_or_else(|e| panic!("form {form}: emit: {e:?}"));
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("form {form}: verify: {e:?}"));
        for (arm, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("form {form}: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
            let mut out_val = vec![0u64; nrows];
            let mut out_valid = vec![0u8; nrows];
            let mut out_err = vec![0u64; nrows];
            let mut ptrs = [core::ptr::null::<u8>(); 4];
            ptrs[0] = base;
            let mut frame = PcFrame { codes: codes.as_ptr(), valid: valid.as_ptr(), nrows: nrows as u64, out_val: out_val.as_mut_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr(), tables: ptrs };
            // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
            let r = unsafe { code.call((&mut frame as *mut PcFrame).cast()) };
            assert_eq!(r, 0);
            for i in 0..nrows {
                let e = &dict.0[codes[i] as usize];
                let want = if chars { chars_of(e) } else { e.len() as u32 } as i64;
                let row_valid = valid[i / 64] >> (i % 64) & 1 == 1;
                assert_eq!(out_err[i], 0, "form {form} {arm} row {i}: no raise");
                assert_eq!(out_valid[i] != 0, row_valid, "form {form} {arm} row {i}: validity");
                if row_valid {
                    assert_eq!(out_val[i] as i32 as i64, want, "form {form} {arm} row {i}: {}", String::from_utf8_lossy(e));
                }
            }
        }
    }
}

/// X5 (PS1-11 / PS1-31): a statement parameter in a code set (`x = $1`, `$2 <> x`, a generic plan's `x IN ($1, 'c', $2)`
/// and `x NOT IN ($1)` as `ARRAY[..]`) is one `Bits` root, a bound code set (`Known::EqBound`: the hosts' EQ table over the
/// constants and the values the source reads per execution); an `ARRAY[..]` with an element of another kind is no root.
#[test]
fn percode_bound_code_sets_x5() {
    install_seams();
    let cx = MemoryContext::new("percode-x5");
    let mcx = cx.mcx();
    fn text_arr<'m>(m: Mcx<'m>, elems: &[Node<'m>]) -> Node<'m> {
        Node::mk(m, types_nodes::primnodes::ArrayExpr { array_typeid: TEXTARRAYOID, array_collid: 950, element_typeid: TEXTOID, elements: NodeList::from_slice(m, elems).unwrap(), multidims: false, list_start: -1, list_end: -1, location: -1 }).unwrap()
    }
    let bound = |values: &[&str], params: &[i32], negated| Some(Known::EqBound { values: mcx::CollectIn::collect_in(values.iter().map(|v| mcx::to_vec_in(v.as_bytes(), emit_cx())), emit_cx()), params: mcx::CollectIn::collect_in(params.iter().map(|&i| (i, TEXTOID)), emit_cx()), consts: mcx::PgVec::new_in(emit_cx()), negated });
    type Build = Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>;
    let cases: Vec<(&str, Build, Option<Known>)> = vec![
        ("x = $1", Box::new(|m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])), bound(&[], &[1], false)),
        ("$2 <> x", Box::new(|m| opexpr(m, F_TEXTNE, BOOLOID, 950, &[param(m, 2, TEXTOID), scan_var(m, 1, TEXTOID)])), bound(&[], &[2], true)),
        ("NOT (x = $1)", Box::new(|m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])])), bound(&[], &[1], true)),
        ("x IN ($1, 'c', $2)", Box::new(|m| saop_by(m, 98, F_TEXTEQ, 950, true, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID), text_const(m, "c"), param(m, 2, TEXTOID)]))), bound(&["c"], &[1, 2], false)),
        ("x NOT IN ($1)", Box::new(|m| saop_by(m, 531, F_TEXTNE, 950, false, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID)]))), bound(&[], &[1], true)),
        ("x = ALL (ARRAY[$1]) is no root", Box::new(|m| saop_by(m, 98, F_TEXTEQ, 950, false, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID)]))), None),
        ("x IN ($1, lower(..)) is no root", Box::new(|m| saop_by(m, 98, F_TEXTEQ, 950, true, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID), scan_var(m, 2, TEXTOID)]))), None),
    ];
    for (name, build, known) in cases {
        let mut compiled = Expr::compile(lists(), build(mcx), &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: compile: {e}"));
        let plan = compiled.percode(&[CodeCol { attno: 1 }], scratch()).unwrap_or_else(|e| panic!("{name}: percode: {e:?}"));
        match known {
            Some(k) => {
                assert_eq!(plan.roots.iter().map(|r| r.form).collect::<Vec<_>>(), vec![Form::Bits], "{name}: roots");
                assert_eq!(plan.needs.iter().map(|n| dup(&n.known)).collect::<Vec<_>>(), vec![k], "{name}: known");
            }
            None => assert!(!plan.needs.iter().any(|n| matches!(n.known, Known::EqBound { .. } | Known::Body)), "{name}: no bound set, no body: {:?}", plan.needs.iter().map(|n| &n.known).collect::<Vec<_>>()),
        }
    }
}

/// PS1-15: a LIKE whose pattern is a statement parameter or `textcat` of parameters and text constants (`x LIKE $1`,
/// `x NOT LIKE '%' || $1 || '%'`, `x ILIKE $1 || '%'`) is one `Bits` root, a bound LIKE (`Known::LikeBound`: the hosts'
/// LIKE table over the pattern the source assembles per execution); a pattern with a column, a non-text function or a
/// NULL constant is none (D1938 / D3521: a UTF-8 database, the only one pgrust serves).
#[test]
fn percode_bound_like_ps115() {
    install_seams();
    let cx = MemoryContext::new("percode-ps115");
    let mcx = cx.mcx();
    const F_TEXTICLIKE: Oid = 1633;
    fn cat<'m>(m: Mcx<'m>, a: Node<'m>, b: Node<'m>) -> Node<'m> {
        opexpr(m, 1258, TEXTOID, 950, &[a, b]) // textcat
    }
    let bound = |parts: &[&str], params: &[i32], ilike, negated| Some(Known::LikeBound { parts: mcx::CollectIn::collect_in(parts.iter().map(|v| mcx::to_vec_in(v.as_bytes(), emit_cx())), emit_cx()), params: mcx::CollectIn::collect_in(params.iter().map(|&i| (i, TEXTOID)), emit_cx()), ilike, negated });
    type Build = Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>;
    let cases: Vec<(&str, Build, Option<Known>)> = vec![
        ("x LIKE $1", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])), bound(&["", ""], &[1], false, false)),
        ("x NOT LIKE $2", Box::new(|m| opexpr(m, F_TEXTNLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 2, TEXTOID)])), bound(&["", ""], &[2], false, true)),
        ("NOT (x LIKE $1)", Box::new(|m| boolexpr(m, BoolExprType::NOT_EXPR, &[opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])])), bound(&["", ""], &[1], false, true)),
        ("x LIKE '%' || $1 || '%'", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, cat(m, text_const(m, "%"), param(m, 1, TEXTOID)), text_const(m, "%"))])), bound(&["%", "%"], &[1], false, false)),
        ("x LIKE $1 || 'a' || 'b' || $2", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, cat(m, cat(m, param(m, 1, TEXTOID), text_const(m, "a")), text_const(m, "b")), param(m, 2, TEXTOID))])), bound(&["", "ab", ""], &[1, 2], false, false)),
        ("x ILIKE $1 || '%'", Box::new(|m| opexpr(m, F_TEXTICLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, param(m, 1, TEXTOID), text_const(m, "%"))])), bound(&["", "%"], &[1], true, false)),
        ("x LIKE $1 || x is no bound LIKE", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, param(m, 1, TEXTOID), scan_var(m, 1, TEXTOID))])), None),
        ("x LIKE lower($1) is no bound LIKE", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), funcexpr(m, 870, TEXTOID, 950, &[param(m, 1, TEXTOID)])])), None),
        ("x LIKE '%' || NULL is no bound LIKE", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, konst(m, TEXTOID, -1, false, None), param(m, 1, TEXTOID))])), None),
    ];
    gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
    for (name, build, known) in &cases {
        let mut compiled = Expr::compile(lists(), build(mcx), &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: compile: {e}"));
        let plan = compiled.percode(&[CodeCol { attno: 1 }], scratch()).unwrap_or_else(|e| panic!("{name}: percode: {e:?}"));
        match known {
            Some(k) => {
                assert_eq!(plan.roots.iter().map(|r| r.form).collect::<Vec<_>>(), vec![Form::Bits], "{name}: roots");
                assert_eq!(plan.needs.iter().map(|n| dup(&n.known)).collect::<Vec<_>>(), vec![dup(k)], "{name}: known");
            }
            None => assert!(!plan.needs.iter().any(|n| matches!(n.known, Known::LikeBound { .. })), "{name}: no bound LIKE: {:?}", plan.needs.iter().map(|n| &n.known).collect::<Vec<_>>()),
        }
    }
}

#[repr(C)]
struct PcNullFrame {
    codes: *const u32,
    valid: *const u64,
    nrows: u64,
    words: *const u64,
    faults: *const u64,
    consts_null: *const u8,
    out_valid: *mut u8,
    out_err: *mut u64,
}

/// The row loop of `percode_build` over one `Bits` table read with its fault bitmap, and `nconsts` statement words whose
/// null flags the frame carries (`RowRefs::consts_null`; their values unread).
fn percode_null_build(c: &Compiled<'_>, nconsts: usize) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    use gravity_abi::ctx::lane;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("percode_null_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let [ptr, valid, nrows, words, faults, cnp, out_valid, out_err] = [0, 8, 16, 24, 32, 40, 48, 56].map(|o| b.load(if o == 16 { Type::I64 } else { Type::Ptr }, ctx, o));
    let (zero, one) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 1));
    let consts = vec![zero; nconsts];
    let consts_null: Vec<Value> = (0..nconsts).map(|j| { let n = b.load_zext(Type::I8, cnp, j as i32); b.icmp(IntCond::Ne, n, zero) }).collect();
    let tables = [TableRef::Bits { words, faults: Some(faults) }];
    let (l_head, l_row, l_out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]));
    b.mark_loop_header(l_head);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let i = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, i, nrows);
    b.cond_br(done, (l_out, &[]), (l_row, &[]));
    b.switch_to(l_row);
    let lane = LaneRef { ptr, valid, valid_null: None, width: 4, kind: lane::KIND_CODES, all_valid: false };
    let codes = [CodeRef { attno: 1, lane, code: None, ordinal: 0, part: None }];
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: i, lanes: &[] }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: None, cells: None, shreds: &[], codes: &codes, tables: &tables, rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = c.emit(&mut b, &row)?;
    let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let ovp = b.ptr_add(out_valid, i, 1, 0);
    b.store(ovp, 0, valid8);
    let oep = b.ptr_add(out_err, i, 8, 0);
    b.store(oep, 0, l.err);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_head, &[i1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

/// PS1-15 / X5: a bound root whose parameter is NULL (its table `Faults`, as the source gives it: every code's fault bit
/// set, no data) reads NULL on every row, a NULL row too, with no fault raised: a LIKE over a NULL pattern part, a code
/// set whose every value is a NULL parameter. The same table under non-NULL parameters raises the fault on every non-NULL
/// row (the row evaluates the body), and so does an IN list whose constant stays beside its NULL parameter (`x IN ($1,
/// 'c')`: true for 'c', NULL elsewhere, the body's answer). TPDE and the interpreter.
#[test]
fn percode_bound_null_reads_null_ps115() {
    install_seams();
    let cx = MemoryContext::new("percode-null");
    let mcx = cx.mcx();
    fn cat<'m>(m: Mcx<'m>, a: Node<'m>, b: Node<'m>) -> Node<'m> {
        opexpr(m, 1258, TEXTOID, 950, &[a, b]) // textcat
    }
    fn text_arr<'m>(m: Mcx<'m>, elems: &[Node<'m>]) -> Node<'m> {
        Node::mk(m, types_nodes::primnodes::ArrayExpr { array_typeid: TEXTARRAYOID, array_collid: 950, element_typeid: TEXTOID, elements: NodeList::from_slice(m, elems).unwrap(), multidims: false, list_start: -1, list_end: -1, location: -1 }).unwrap()
    }
    type Build = Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>;
    // (case, its NULL parameters read NULL without the table)
    let cases: Vec<(&str, Build, bool)> = vec![
        ("x LIKE $1", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])), true),
        ("x NOT LIKE '%' || $1 || '%'", Box::new(|m| opexpr(m, F_TEXTNLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, cat(m, text_const(m, "%"), param(m, 1, TEXTOID)), text_const(m, "%"))])), true),
        ("x LIKE $1 || $2", Box::new(|m| opexpr(m, F_TEXTLIKE, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), cat(m, param(m, 1, TEXTOID), param(m, 2, TEXTOID))])), true),
        ("x = $1", Box::new(|m| opexpr(m, F_TEXTEQ, BOOLOID, 950, &[scan_var(m, 1, TEXTOID), param(m, 1, TEXTOID)])), true),
        ("$2 <> x", Box::new(|m| opexpr(m, F_TEXTNE, BOOLOID, 950, &[param(m, 2, TEXTOID), scan_var(m, 1, TEXTOID)])), true),
        ("x NOT IN ($1, $2)", Box::new(|m| saop_by(m, 531, F_TEXTNE, 950, false, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID), param(m, 2, TEXTOID)]))), true),
        ("x IN ($1, 'c') keeps the fault", Box::new(|m| saop_by(m, 98, F_TEXTEQ, 950, true, scan_var(m, 1, TEXTOID), text_arr(m, &[param(m, 1, TEXTOID), text_const(m, "c")]))), false),
    ];
    gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
    let (codes, valid) = percode_rows();
    let nrows = codes.len();
    let (words, faults) = ([0u64; 1], [u64::MAX; 1]); // the fixture's 8 codes: no data, every fault bit
    for (name, build, reads_null) in &cases {
        let mut compiled = Expr::compile(lists(), build(mcx), &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: compile: {e}"));
        let plan = compiled.percode(&[CodeCol { attno: 1 }], scratch()).unwrap_or_else(|e| panic!("{name}: percode: {e:?}"));
        assert_eq!(plan.roots.iter().map(|r| r.form).collect::<Vec<_>>(), vec![Form::Bits], "{name}: roots");
        assert!(plan.needs.iter().all(|n| matches!(n.known, Known::EqBound { .. } | Known::LikeBound { .. })), "{name}: a bound root");
        let needs = compiled.param_needs(scratch()).unwrap();
        let (ir, f) = percode_null_build(&compiled, needs.len()).unwrap_or_else(|e| panic!("{name}: emit: {e:?}"));
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name}: verify: {e:?}"));
        let tpde = gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{name}: tpde: {}", e.message()));
        let interp = gravity_backend::interp(ir, f).unwrap();
        for null in [true, false] {
            let consts_null: Vec<u8> = needs.iter().map(|n| (null && matches!(n, ParamNeed::Extern { .. })) as u8).collect();
            for (arm, code) in [("tpde", &tpde), ("interp", &interp)] {
                let (mut out_valid, mut out_err) = (vec![9u8; nrows], vec![0u64; nrows]);
                let mut frame = PcNullFrame { codes: codes.as_ptr(), valid: valid.as_ptr(), nrows: nrows as u64, words: words.as_ptr(), faults: faults.as_ptr(), consts_null: consts_null.as_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr() };
                // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
                assert_eq!(unsafe { code.call((&mut frame as *mut PcNullFrame).cast()) }, 0);
                for r in 0..nrows {
                    let row_null = valid[r / 64] >> (r % 64) & 1 == 0;
                    let (v, e) = (out_valid[r], out_err[r]);
                    if null && *reads_null {
                        assert_eq!((v, e), (0, 0), "{name} {arm}: a NULL parameter, row {r}: NULL, no fault");
                    } else if row_null {
                        assert_eq!(e, 0, "{name} {arm}: a NULL row raises nothing, row {r}");
                    } else {
                        assert_ne!(e, 0, "{name} {arm}: the fault kept (null parameters {null}), row {r}");
                    }
                }
            }
        }
    }
}

/// interp = TPDE = the incumbent = `eval` over the fixture's rows for every per-code case; the
/// tables filled by `percode::fill` (or the R4 index view), the analysis' roots and known shapes asserted.
#[test]
fn percode_matches_eval_the_incumbent_the_interpreter_and_tpde() {
    install_seams();
    // a statement runs under its database's encoding, UTF8 (D3521): a test thread boots as SQL_ASCII
    gravity_fn::typesem::collation::arm_database_encoding(6).expect("UTF8");
    let cx = MemoryContext::new("percode-test");
    let mcx = cx.mcx();
    let dict = percode_dict();
    let (codes, valid) = percode_rows();
    let index = percode_index(&dict);
    let mut total = 0;
    for c in pc_cases() {
        let expr = (c.build)(mcx);
        let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{}: compile: {e}", c.name));
        let cols = [CodeCol { attno: 1 }];
        let plan = compiled.percode(&cols, scratch()).unwrap_or_else(|e| panic!("{}: percode: {e:?}", c.name));
        assert_eq!(plan.roots.iter().map(|r| r.form).collect::<Vec<_>>(), c.forms, "{}: roots", c.name);
        // NEW-72 entry C: a constant compare is a bound table, never one with the literal baked (`''`, which both keys
        // hold, is the empty entry's set as before)
        assert!(!plan.needs.iter().any(|n| matches!(&n.known, Known::Eq { values, .. } if values.iter().any(|v| !v.is_empty())) || matches!(n.known, Known::Range { .. })), "{}: a baked literal: {:?}", c.name, plan.needs.iter().map(|n| &n.known).collect::<Vec<_>>());
        assert_eq!(plan.needs.iter().map(|n| bound_as_baked(&compiled, &n.known)).collect::<Vec<_>>(), c.known, "{}: known", c.name);
        // the tables: the fill per need, or the index view for a length
        let mut bufs: Vec<Vec<u64>> = Vec::new();
        let mut forms = Vec::new();
        let mut ptrs = [core::ptr::null::<u8>(); 4];
        for (i, need) in plan.needs.iter().enumerate() {
            if c.idx {
                assert_eq!(need.known, Known::OctetLength);
                forms.push(PcForm::IdxLen);
                // the header's `data` of an index view is the entry's length fields (`index_len_header`)
                ptrs[i] = unsafe { index.as_ptr().add(4) };
                continue;
            }
            let mut buf = vec![0u64; 1 << 12];
            let mut region = unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
            let mut scratch = vec![0u8; 1 << 12];
            let hdr = fill(&mut compiled, need, &dict, &mut region, &mut scratch).unwrap_or_else(|e| panic!("{}: fill: {e:?}", c.name));
            let h = unsafe { &*hdr };
            assert_eq!((h.elem, h.ncodes, h.faults.is_null()), (need.elem, dict.0.len() as u32, true), "{}: header", c.name);
            ptrs[i] = h.data;
            forms.push(if need.elem == gravity_dict::ELEM_BITS { PcForm::Bits } else { PcForm::Words });
            bufs.push(buf);
        }
        let (ir, f) = percode_build(&compiled, &forms, false).unwrap_or_else(|e| panic!("{}: emit: {e:?}", c.name));
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", c.name));
        // the truth form of a boolean root (§7.7): the same verdicts from `emit_pred`'s `cond`
        let pred = (c.typ == BOOLOID).then(|| {
            let (ir, f) = percode_build(&compiled, &forms, true).unwrap_or_else(|e| panic!("{}: emit_pred: {e:?}", c.name));
            gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify (pred): {e:?}", c.name));
            (ir, f)
        });
        let nrows = codes.len();
        let run = |code: gravity_program::Code| -> Vec<Outcome> {
            let mut out_val = vec![0u64; nrows];
            let mut out_valid = vec![0u8; nrows];
            let mut out_err = vec![0u64; nrows];
            let mut frame = PcFrame { codes: codes.as_ptr(), valid: valid.as_ptr(), nrows: nrows as u64, out_val: out_val.as_mut_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr(), tables: ptrs };
            // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
            let r = unsafe { code.call((&mut frame as *mut PcFrame).cast()) };
            assert_eq!(r, 0);
            (0..nrows)
                .map(|i| {
                    if out_err[i] != 0 {
                        raised(out_err[i])
                    } else if out_valid[i] == 0 {
                        Outcome::Null
                    } else if c.typ == TEXTOID {
                        Outcome::Value(out_val[i].to_ne_bytes().to_vec())
                    } else {
                        bytes_of(NullableDatum { value: Datum::from_u64(out_val[i]), isnull: false }, c.typ)
                    }
                })
                .collect()
        };
        let tpde = run(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde: {}", c.name, e.message())));
        let interp = run(gravity_backend::interp(ir, f).unwrap());
        // the references: `eval` and the incumbent over the row's text datum
        let mut rbuf = vec![0u8; 1 << 16];
        let mut region = Region::over(&mut rbuf);
        let mut want = Vec::new();
        for i in 0..nrows {
            if valid[i / 64] >> (i % 64) & 1 == 0 {
                want.push(Outcome::Null);
                continue;
            }
            if c.typ == TEXTOID {
                want.push(Outcome::Value((codes[i] as u64).to_ne_bytes().to_vec()));
                continue;
            }
            let d = text_image(mcx, std::str::from_utf8(&dict.0[codes[i] as usize]).unwrap());
            let values = [d];
            let nulls = [false];
            let r = Row::scan_typed(&values, &nulls, &[TEXTOID], 0);
            let e = match compiled.eval(&r, &mut region) {
                Ok(v) => bytes_of(v, c.typ),
                Err(e) => err_of(&e),
            };
            let expr = (c.build)(mcx);
            let mut state = exec_init_expr(mcx, Some(expr), ParamBind::NONE).unwrap().unwrap();
            state.arm_result_mcx(mcx);
            let mut slot = virtual_slot(mcx, &[TEXTOID], &[Some(d.as_u64())]);
            let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
            let inc = match exec_eval_expr(&mut state, &mut slots) {
                Ok(v) => bytes_of(pg_value(v, c.typ), c.typ),
                Err(e) => err_of(&e),
            };
            assert_eq!(e, inc, "{}: eval vs the incumbent, row {i}", c.name);
            want.push(e);
        }
        assert_eq!(tpde, want, "{}: tpde", c.name);
        assert_eq!(interp, want, "{}: interp", c.name);
        assert!(want.iter().any(|o| matches!(o, Outcome::Value(_))), "{}: a value somewhere", c.name);
        if let Some((ir, f)) = pred {
            let tpde = run(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde (pred): {}", c.name, e.message())));
            assert_eq!(tpde, want, "{}: tpde, the truth form", c.name);
            assert_eq!(run(gravity_backend::interp(ir, f).unwrap()), want, "{}: interp, the truth form", c.name);
            total += nrows;
        }
        total += nrows;
    }
    eprintln!("percode: {} cases, {total} rows, interp = tpde = incumbent = eval", pc_cases().len());
}

/// The refusals (§7.1): two code lanes in one body, a non-strict body over the code lane, a
/// by-reference result as a row value, an undeclared code lane, an unbound table.
#[test]
fn percode_refusals() {
    install_seams();
    let cx = MemoryContext::new("percode-refusals");
    let mcx = cx.mcx();
    let cols = [CodeCol { attno: 1 }, CodeCol { attno: 2 }];
    // `x = y` over two code lanes: no root; the Vars stay code lanes and emit refuses them
    let c = Expr::compile(lists(), opexpr(mcx, F_TEXTEQ, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), scan_var(mcx, 2, TEXTOID)]), &PgCatalog, ParamBind::NONE).unwrap();
    assert!(c.percode(&cols, scratch()).unwrap().roots.is_empty());
    assert_eq!(percode_build(&c, &[], false).err(), Some(EmitError::CodesLane(1)));
    // `x IS NULL`: not strict in the lane → no root
    let c = Expr::compile(lists(), null_test(mcx, scan_var(mcx, 1, TEXTOID), NullTestType::IS_NULL), &PgCatalog, ParamBind::NONE).unwrap();
    assert!(c.percode(&cols, scratch()).unwrap().roots.is_empty());
    // `x LIKE 'a' OR x LIKE 'b'`: the OR is not strict; its operands are the roots
    let c = Expr::compile(lists(), boolexpr(mcx, BoolExprType::OR_EXPR, &[opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), text_const(mcx, "a%")]), opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), text_const(mcx, "b%")])]), &PgCatalog, ParamBind::NONE).unwrap();
    let plan = c.percode(&cols, scratch()).unwrap();
    assert_eq!((plan.roots.len(), plan.needs.len()), (2, 2));
    assert_ne!(plan.needs[0].fingerprint, plan.needs[1].fingerprint);
    // `upper(x)` as a value: a derived root, refused by the row form
    let c = Expr::compile(lists(), funcexpr(mcx, F_UPPER, TEXTOID, 950, &[scan_var(mcx, 1, TEXTOID)]), &PgCatalog, ParamBind::NONE).unwrap();
    assert_eq!(c.percode(&cols, scratch()).unwrap().roots[0].form, Form::Derived);
    assert_eq!(percode_build(&c, &[], false).err(), Some(EmitError::PerCodeByRef));
    // a table the caller did not bind
    let c = Expr::compile(lists(), opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), text_const(mcx, "%g%")]), &PgCatalog, ParamBind::NONE).unwrap();
    assert_eq!(percode_build(&c, &[], false).err(), Some(EmitError::NoTable(0)));
    // `x LIKE NULL`: a null constant is never per-code
    let c = Expr::compile(lists(), opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), konst(mcx, TEXTOID, -1, false, None)]), &PgCatalog, ParamBind::NONE).unwrap();
    assert!(c.percode(&cols, scratch()).unwrap().roots.is_empty());
    // the same body has the same fingerprint; another pattern another
    let f = |pat: &str| Expr::compile(lists(), opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), text_const(mcx, pat)]), &PgCatalog, ParamBind::NONE).unwrap().table_needs(&cols, scratch()).unwrap()[0].fingerprint;
    assert_eq!(f("%g%"), f("%g%"));
    assert_ne!(f("%g%"), f("%h%"));
}

/// The fill's cost against the hand matcher on a 200 k-entry URL corpus (`PERCODE_FILL_COST=1`).
#[test]
fn percode_fill_cost() {
    if std::env::var_os("PERCODE_FILL_COST").is_none() {
        return;
    }
    install_seams();
    let cx = MemoryContext::new("percode-fill-cost");
    let mcx = cx.mcx();
    let hosts = ["example.com", "shop.example.org", "news.site.ru", "m.video.net", "images.host.io", "garden.com"];
    let paths = ["", "/", "/index.html", "/search?q=", "/item/", "/user/profile", "/cat/sub/leaf"];
    let mut x = 0x9E37_79B9u64;
    let mut ents: Vec<Vec<u8>> = (0..200_000u32)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("http://{}{}{i}{}", hosts[(x % 6) as usize], paths[((x >> 8) % 7) as usize], if x & 0x100 != 0 { "&utm_source=Garden" } else { "" }).into_bytes()
        })
        .collect();
    ents.sort();
    ents.dedup();
    let dict = VecDict(ents);
    let mut compiled = Expr::compile(lists(), opexpr(mcx, F_TEXTLIKE, BOOLOID, 950, &[scan_var(mcx, 1, TEXTOID), text_const(mcx, "%garden%")]), &PgCatalog, ParamBind::NONE).unwrap();
    let need = compiled.table_needs(&[CodeCol { attno: 1 }], scratch()).unwrap().remove(0);
    let mut buf = vec![0u64; 1 << 16];
    let mut scratch = vec![0u8; 1 << 12];
    for _ in 0..3 {
        let mut region = unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
        let t0 = std::time::Instant::now();
        let hdr = fill(&mut compiled, &need, &dict, &mut region, &mut scratch).unwrap();
        let fill_us = t0.elapsed().as_micros();
        let mut region2 = unsafe { gravity_dict::Region::new(buf.as_mut_ptr().add(1 << 15) as *mut u8, 1 << 18) };
        let t0 = std::time::Instant::now();
        let m = gravity_dict::like::Matcher::new(b"%garden%", gravity_dict::like::Mode::Like, &mut region2).unwrap();
        let bits = m.build_bits(&dict, &mut region2).unwrap();
        let hand_us = t0.elapsed().as_micros();
        let a = unsafe { gravity_dict::Bits::at(hdr) };
        let same = (0..dict.0.len() as u32).all(|c| unsafe { a.get(c) == bits.get(c) });
        eprintln!("percode fill (eval per entry) {fill_us} us vs Matcher::build_bits {hand_us} us over {} entries; equal {same}", dict.0.len());
        assert!(same);
    }
}
/// The vector form's frame (§8.3): three all-valid lanes, the constants, the vector finals and
/// the `err` verdict, the scalar rows' value / valid / err, the words' masks.
#[repr(C)]
struct VecFrame {
    lanes: [gravity_abi::ctx::Lane; 3],
    rows: u64,
    consts: *const u64,
    consts_null: *const u8,
    out: *mut u64,
    row_val: *mut u64,
    row_flags: *mut u8,
    words: *mut u64,
}

const VF_ROWS: i32 = 3 * gravity_abi::ctx::lane::SIZE as i32;

/// The function: the chunk loop under the fits guard (`out[0..2]` the final, `out[2]` the `err`
/// verdict, `out[3]` the guard), the same rows through `emit` (`row_val`, `row_flags` = valid | err << 1),
/// a boolean root's `emit_word` per word (`words`).
fn vec_program(c: &Compiled<'_>, widths: [u8; 3], nconsts: usize, agg: VecAgg) -> Result<(gravity_ir::Program, gravity_ir::Func, Type, bool), EmitError> {
    use gravity_abi::ctx::lane;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("emit_vec_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let rows = b.load(Type::I64, ctx, VF_ROWS);
    let cp = b.load(Type::Ptr, ctx, VF_ROWS + 8);
    let cnp = b.load(Type::Ptr, ctx, VF_ROWS + 16);
    let out = b.load(Type::Ptr, ctx, VF_ROWS + 24);
    let row_val = b.load(Type::Ptr, ctx, VF_ROWS + 32);
    let row_flags = b.load(Type::Ptr, ctx, VF_ROWS + 40);
    let words = b.load(Type::Ptr, ctx, VF_ROWS + 48);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let mut consts = Vec::new();
    let mut consts_null = Vec::new();
    for j in 0..nconsts {
        consts.push(b.load(Type::I64, cp, (j * 8) as i32));
        let n = b.load_zext(Type::I8, cnp, j as i32);
        consts_null.push(b.icmp(IntCond::Ne, n, zero));
    }
    let mut lanes: Vec<Option<LaneRef>> = Vec::new();
    for (l, &w) in widths.iter().enumerate() {
        let off = (l * lane::SIZE) as i32;
        let ptr = b.load(Type::Ptr, ctx, off + lane::PTR as i32);
        let valid = b.load(Type::Ptr, ctx, off + lane::VALID as i32);
        lanes.push(Some(LaneRef { ptr, valid, valid_null: None, width: w, kind: lane::KIND_BYVAL, all_valid: true }));
    }
    let w = WordRefs { scratch: scratch(), lanes: &lanes, consts: &consts, consts_null: &consts_null, tables: &[] };
    let mut e: VecEmit = c.emit_vec(&mut b, &w)?;
    let sty = agg.state_type(&e, 8192).ok_or(EmitError::NotVector)?;
    let ok = e.ok(&mut b, &w);
    b.store(out, 24, ok);
    let init = agg.init(&mut b, sty);
    let (lty, lanes_n) = (e.ty, e.ty.lanes());
    // two accumulator sets (chunk 0 → A, chunk 1 → B per step), combined at the tail: `combine` under test
    let l_vec = b.create_block(&[Type::I64, sty, sty, sty, sty, lty]);
    let l_body = b.create_block(&[]);
    let l_tail = b.create_block(&[Type::I64, sty, sty, sty, sty, lty]);
    let l_rows = b.create_block(&[Type::I64]);
    let l_row = b.create_block(&[]);
    let l_words = b.create_block(&[Type::I64]);
    let l_word = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_vec);
    b.mark_loop_header(l_rows);
    b.mark_loop_header(l_words);
    let zl = b.iconst(lty.lane(), 0);
    let zv = b.vsplat(lty, zl);
    let init_hi = init.hi.unwrap_or(init.lo);
    let okb = b.icmp(IntCond::Ne, ok, zero);
    b.cond_br(okb, (l_vec, &[zero, init.lo, init_hi, init.lo, init_hi, zv]), (l_tail, &[zero, init.lo, init_hi, init.lo, init_hi, zv]));
    b.switch_to(l_vec);
    let p = b.block_params(l_vec).to_vec();
    let (j, dirty) = (p[0], p[5]);
    let step = b.iconst(Type::I64, 2 * e.rows as i64);
    let j2 = b.int_bin(IntBinOp::Add, j, step);
    let fits = b.icmp(IntCond::Ule, j2, rows);
    b.cond_br(fits, (l_body, &[]), (l_tail, &p));
    b.switch_to(l_body);
    let mut next = vec![j2];
    let mut dirty2 = dirty;
    for k in 0..2u32 {
        let chunk = e.chunk(&mut b, &w, j, k);
        let (lo, hi) = (p[1 + 2 * k as usize], p[2 + 2 * k as usize]);
        let acc = agg.transition(&mut b, VecAcc { lo, hi: init.hi.map(|_| hi), ty: sty }, &chunk);
        next.push(acc.lo);
        next.push(acc.hi.unwrap_or(acc.lo));
        if let Some(err) = chunk.err {
            dirty2 = b.vbin(gravity_ir::VecBinOp::Or, dirty2, err);
        }
    }
    next.push(dirty2);
    b.br(l_vec, &next);
    b.switch_to(l_tail);
    let p = b.block_params(l_tail).to_vec();
    let (jt, dirty) = (p[0], p[5]);
    let acc_a = VecAcc { lo: p[1], hi: init.hi.map(|_| p[2]), ty: sty };
    let acc_b = VecAcc { lo: p[3], hi: init.hi.map(|_| p[4]), ty: sty };
    let combined = agg.combine(&mut b, acc_a, acc_b);
    let fin = agg.finish(&mut b, combined);
    b.store(out, 0, fin.value);
    let fh = fin.hi.unwrap_or(zero);
    b.store(out, 8, fh);
    let any = b.vreduce_add(dirty);
    b.store(out, 16, any);
    b.store(out, 32, jt);
    b.br(l_rows, &[zero]);
    // the scalar rows
    b.switch_to(l_rows);
    let i = b.block_params(l_rows)[0];
    let done = b.icmp(IntCond::Uge, i, rows);
    b.cond_br(done, (l_words, &[zero]), (l_row, &[]));
    b.switch_to(l_row);
    let row = RowRefs { scratch: scratch(), scan: ColsRef::Lanes { row: i, lanes: &lanes }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: None, cells: None, shreds: &[], codes: &[], tables: &[], rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
    let l = c.emit(&mut b, &row)?;
    let vp = b.ptr_add(row_val, i, 8, 0);
    b.store(vp, 0, l.value);
    let v8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let z64 = b.iconst(Type::I64, 0);
    let raised = b.icmp(IntCond::Ne, l.err, z64);
    let e8 = b.conv(ConvOp::Zext, Type::I8, raised);
    let one8 = b.iconst(Type::I8, 1);
    let e8s = b.int_bin(IntBinOp::Shl, e8, one8);
    let fl = b.int_bin(IntBinOp::Or, v8, e8s);
    let fp = b.ptr_add(row_flags, i, 1, 0);
    b.store(fp, 0, fl);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_rows, &[i1]);
    // the words of a boolean root
    b.switch_to(l_words);
    let wi = b.block_params(l_words)[0];
    let six = b.iconst(Type::I64, 6);
    let nwords = b.int_bin(IntBinOp::Ushr, rows, six);
    let wdone = b.icmp(IntCond::Uge, wi, nwords);
    b.cond_br(wdone, (l_out, &[]), (l_word, &[]));
    b.switch_to(l_word);
    let base = b.int_bin(IntBinOp::Shl, wi, six);
    let (m, words_ok) = match (e.mask, c.emit_word(&mut b, &w, base)) {
        (true, Ok(m)) => (m, true),
        (_, Err(EmitError::NotVector)) | (false, _) => (zero, false),
        (_, Err(e)) => return Err(e),
    };
    let n = b.int_un(IntUnOp::Popcnt, m);
    let wp = b.ptr_add(words, wi, 8, 0);
    b.store(wp, 0, n);
    let wi1 = b.int_bin(IntBinOp::Add, wi, one);
    b.br(l_words, &[wi1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    let _ = lanes_n;
    Ok((pb.finish(), f, sty, words_ok))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fold {
    value: i128,
    err: bool,
}

/// The scalar fold of an aggregate over (value, valid, err) rows: the first `rows` of them.
fn fold(agg: VecAgg, rows: &[(u64, bool, bool)], width: u8) -> Fold {
    let sext = |v: u64| -> i64 {
        match width {
            1 => v as u8 as i64,
            2 => v as i16 as i64,
            4 => v as i32 as i64,
            _ => v as i64,
        }
    };
    let mut err = false;
    let mut acc: Option<i128> = None;
    for &(v, valid, e) in rows {
        err |= e;
        assert!(valid || e, "an all-valid window has no null row");
        if e {
            continue;
        }
        let x = sext(v) as i128;
        acc = Some(match (agg, acc) {
            (VecAgg::Count, a) => a.unwrap_or(0) + (v != 0) as i128,
            (VecAgg::Sum | VecAgg::Sum128, a) => a.unwrap_or(0) + x,
            (VecAgg::Min, None) | (VecAgg::Max, None) => x,
            (VecAgg::Min, Some(a)) => a.min(x),
            (VecAgg::Max, Some(a)) => a.max(x),
            _ => unreachable!(),
        });
    }
    Fold { value: acc.unwrap_or(0), err }
}

#[test]
fn emit_vec_matches_the_scalar_fold_eval_and_the_incumbent() {
    install_seams();
    let cx = MemoryContext::new("emit-vec-test");
    let mcx = cx.mcx();
    const ROWS: usize = 256;
    let a: Vec<i16> = (0..ROWS).map(|i| ((i as i64 * 7919) % 1000) as i16 - 200).collect();
    let bcol: Vec<i32> = (0..ROWS).map(|i| ((i as i64 * 104729) % 100_000) as i32 - 50_000).collect();
    // the golden-ratio stride: the second accumulator set's low word ends with its top bit set (the pair combine's carry)
    let ccol: Vec<i64> = (0..ROWS).map(|i| (i as i64).wrapping_mul(0x9E3779B97F4A7C15u64 as i64)).collect();
    let all: Vec<u64> = vec![!0; ROWS / 64];
    let lanes = [gravity_abi::ctx::Lane::byval(a.as_ptr().cast(), all.as_ptr(), 2, 0, 0), gravity_abi::ctx::Lane::byval(bcol.as_ptr().cast(), all.as_ptr(), 4, 0, 0), gravity_abi::ctx::Lane::byval(ccol.as_ptr().cast(), all.as_ptr(), 8, 0, 0)];
    let widths = [2u8, 4, 8];
    type Build = Box<dyn Fn(Mcx<'_>) -> Node<'_>>;
    // (name, the program, the lane it reads (width), the aggregates it feeds)
    let shapes: Vec<(&str, Build, u8, Vec<VecAgg>)> = vec![
        ("a", Box::new(|m| scan_var(m, 1, INT2OID)), 2, vec![VecAgg::Sum, VecAgg::Min, VecAgg::Max]),
        ("b", Box::new(|m| scan_var(m, 2, INT4OID)), 4, vec![VecAgg::Sum, VecAgg::Min, VecAgg::Max]),
        ("c", Box::new(|m| scan_var(m, 3, INT8OID)), 8, vec![VecAgg::Sum128, VecAgg::Min, VecAgg::Max]),
        ("a + 5", Box::new(|m| opexpr(m, 176, INT2OID, 0, &[scan_var(m, 1, INT2OID), konst(m, INT2OID, 2, true, Some(Datum::from_i64(5)))])), 2, vec![VecAgg::Sum, VecAgg::Min]),
        ("a - 32700 (overflows)", Box::new(|m| opexpr(m, 180, INT2OID, 0, &[scan_var(m, 1, INT2OID), konst(m, INT2OID, 2, true, Some(Datum::from_i64(32700)))])), 2, vec![VecAgg::Sum]),
        ("32760 + a (overflows)", Box::new(|m| opexpr(m, 176, INT2OID, 0, &[konst(m, INT2OID, 2, true, Some(Datum::from_i64(32760))), scan_var(m, 1, INT2OID)])), 2, vec![VecAgg::Max]),
        ("b - 7", Box::new(|m| opexpr(m, 181, INT4OID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(7))])), 4, vec![VecAgg::Sum, VecAgg::Max]),
        ("a < 100", Box::new(|m| opexpr(m, 64, BOOLOID, 0, &[scan_var(m, 1, INT2OID), konst(m, INT2OID, 2, true, Some(Datum::from_i64(100)))])), 2, vec![VecAgg::Count]),
        ("b = 7 (never)", Box::new(|m| opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(7))])), 4, vec![VecAgg::Count]),
        ("c <> 3", Box::new(|m| opexpr(m, 468, BOOLOID, 0, &[scan_var(m, 3, INT8OID), int8(m, Some(3))])), 8, vec![VecAgg::Count]),
        ("5 < b (commuted)", Box::new(|m| opexpr(m, 66, BOOLOID, 0, &[int4(m, Some(5)), scan_var(m, 2, INT4OID)])), 4, vec![VecAgg::Count]),
        ("b in (7, 300, -49993)", Box::new(|m| saop(m, true, scan_var(m, 2, INT4OID), int4_array_const(m, Some(&[Some(7), Some(300), Some(-49993)])))), 4, vec![VecAgg::Count]),
        ("b >= -100 and b <= 40000", Box::new(|m| boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 150, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(-100))]), opexpr(m, 149, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(40000))])])), 4, vec![VecAgg::Count]),
        ("b < 0 or b > 49000", Box::new(|m| boolexpr(m, BoolExprType::OR_EXPR, &[opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(49000))])])), 4, vec![VecAgg::Count]),
        ("a + 5 < 0", Box::new(|m| opexpr(m, 64, BOOLOID, 0, &[opexpr(m, 176, INT2OID, 0, &[scan_var(m, 1, INT2OID), konst(m, INT2OID, 2, true, Some(Datum::from_i64(5)))]), konst(m, INT2OID, 2, true, Some(Datum::from_i64(0)))])), 2, vec![VecAgg::Count]),
        ("b = 3000000000 (does not fit)", Box::new(|m| opexpr(m, 852, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int8(m, Some(3_000_000_000))])), 4, vec![VecAgg::Count]),
    ];
    let mut cases = 0;
    for (name, build, width, aggs) in &shapes {
        let expr = build(mcx);
        let compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
        let typ = compiled.result_type;
        let needs = compiled.param_needs(scratch()).unwrap();
        let consts: Vec<u64> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.value.as_u64(), _ => 0 }).collect();
        let consts_null: Vec<u8> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.isnull as u8, _ => 1 }).collect();
        // eval and the incumbent per row, folded
        let mut buf = vec![0u8; 4096];
        let mut region = Region::over(&mut buf);
        let mut c2 = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
        let mut ev: Vec<(u64, bool, bool)> = Vec::new();
        let mut inc: Vec<(u64, bool, bool)> = Vec::new();
        let types = [INT2OID, INT4OID, INT8OID];
        for i in 0..ROWS {
            let r = Row { scan: gravity_expr::Cols::Lanes { lanes: &lanes, index: i }, inner: gravity_expr::Cols::None, outer: gravity_expr::Cols::None, aggs: &[] };
            ev.push(match c2.eval(&r, &mut region) {
                Ok(v) => (v.value.as_u64(), !v.isnull, false),
                Err(_) => (0, false, true),
            });
            let mut state = exec_init_expr(mcx, Some(expr), ParamBind::NONE).unwrap().unwrap();
            state.arm_result_mcx(mcx);
            let word = [i2(a[i]), i4(bcol[i]), i8w(ccol[i])];
            let mut slot = virtual_slot(mcx, &types, &word);
            let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
            inc.push(match exec_eval_expr(&mut state, &mut slots) {
                Ok(v) => (v.value.as_u64(), !v.isnull, false),
                Err(_) => (0, false, true),
            });
        }
        let expect_ok = !name.contains("does not fit");
        for &agg in aggs {
            let (ir, f, sty, words_ok) = vec_program(&compiled, widths, needs.len(), agg).unwrap_or_else(|e| panic!("{name} {agg:?}: {e:?}"));
            gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name} {agg:?}: {e:?}"));
            for (arm, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{name}: {}", e.message()))), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
                let mut out = [0u64; 5];
                let mut row_val = vec![0u64; ROWS];
                let mut row_flags = vec![0u8; ROWS];
                let mut words = vec![0u64; ROWS / 64];
                let mut frame = VecFrame { lanes, rows: ROWS as u64, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), out: out.as_mut_ptr(), row_val: row_val.as_mut_ptr(), row_flags: row_flags.as_mut_ptr(), words: words.as_mut_ptr() };
                // SAFETY: the frame is the ctx; every pointer is live for the call.
                let r = unsafe { code.call((&mut frame as *mut VecFrame).cast()) };
                assert_eq!(r, 0);
                assert_eq!(out[3], expect_ok as u64, "{name} {agg:?} {arm}: the fits guard");
                let rows_vec = out[4] as usize;
                assert_eq!(rows_vec, if expect_ok { ROWS } else { 0 }, "{name} {agg:?} {arm}: whole chunks");
                let scalar: Vec<(u64, bool, bool)> = (0..ROWS).map(|i| (row_val[i], row_flags[i] & 1 != 0, row_flags[i] & 2 != 0)).collect();
                let masked = |v: &[(u64, bool, bool)]| -> Vec<(u64, bool, bool)> { v.iter().map(|&(x, valid, e)| (if valid && !e { x & if typ == BOOLOID { 1 } else { !0 } } else { 0 }, valid && !e, e)).collect() };
                assert_eq!(masked(&scalar), masked(&ev), "{name} {arm}: emit per row vs eval");
                assert_eq!(masked(&scalar), masked(&inc), "{name} {arm}: emit per row vs the incumbent");
                let want = fold(agg, &scalar[..rows_vec], if agg == VecAgg::Count { 1 } else { *width });
                let dirty = out[2] as i64 != 0;
                assert_eq!(dirty, want.err, "{name} {agg:?} {arm}: the err verdict");
                if !want.err {
                    let got: i128 = if agg == VecAgg::Sum128 { (((out[1] as u128) << 64) | out[0] as u128) as i128 } else { out[0] as i64 as i128 };
                    assert_eq!(got, want.value, "{name} {agg:?} {arm} ({sty:?}): the vector final vs the scalar fold");
                    if agg == VecAgg::Count && expect_ok && words_ok {
                        assert_eq!(words.iter().sum::<u64>() as i128, want.value, "{name} {arm}: the counting form vs popcount(emit_word)");
                    }
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 2 * shapes.iter().map(|s| s.3.len()).sum::<usize>());
    eprintln!("emit_vec cases: {cases}");
}

// ---- the aggregate cells of a record (expr-compiler.md §9) ----

/// The fixture record: `[count | sum i128 | avg (i128 sum, n) | f64 sum, n | key]`, 72 bytes;
/// the count word may carry the breaker's `MOVING` bit.
const R_COUNT: u32 = 0;
const R_SUM: u32 = 8;
const R_AVG: u32 = 24;
const R_F64: u32 = 48;
const R_KEY: u32 = 64;
const R_SIZE: usize = 72;
const R_MOVING: u64 = 1 << 63;

struct RecRow {
    count: i64,
    moving: bool,
    sum: i128,
    avg: (i128, i64),
    f64: (f64, i64),
    key: i64,
}

fn rec_bytes(r: &RecRow) -> [u8; R_SIZE] {
    let mut b = [0u8; R_SIZE];
    let w = |b: &mut [u8; R_SIZE], off: u32, v: u64| b[off as usize..off as usize + 8].copy_from_slice(&v.to_ne_bytes());
    w(&mut b, R_COUNT, r.count as u64 | if r.moving { R_MOVING } else { 0 });
    w(&mut b, R_SUM, r.sum as u64);
    w(&mut b, R_SUM + 8, (r.sum >> 64) as u64);
    w(&mut b, R_AVG, r.avg.0 as u64);
    w(&mut b, R_AVG + 8, (r.avg.0 >> 64) as u64);
    w(&mut b, R_AVG + 16, r.avg.1 as u64);
    w(&mut b, R_F64, r.f64.0.to_bits());
    w(&mut b, R_F64 + 8, r.f64.1 as u64);
    w(&mut b, R_KEY, r.key as u64);
    b
}

/// The cells as the emitted code names them, by aggno: count, sum as int8, avg numeric, avg float8, sum numeric.
const R_CELLS: [AggCell; 5] = [AggCell::count(R_COUNT), AggCell::sum_i128_as_i64(R_SUM, R_COUNT), AggCell::avg_i128(R_AVG), AggCell::avg_f64(R_F64, R_F64 + 8), AggCell::sum_i128_numeric(R_SUM, R_COUNT)];

fn rec_rows() -> Vec<RecRow> {
    vec![
        RecRow { count: 3, moving: true, sum: 10, avg: (30, 3), f64: (4.5, 3), key: 7 },
        RecRow { count: 0, moving: false, sum: 0, avg: (0, 0), f64: (0.0, 0), key: -1 },
        RecRow { count: 100_001, moving: false, sum: -12_345_678_901_234, avg: (-7, 2), f64: (-1.0, 4), key: 0 },
        RecRow { count: 1, moving: true, sum: i64::MAX as i128, avg: (i64::MAX as i128 * 3, 3), f64: (1e300, 1), key: 5 },
        RecRow { count: 2, moving: false, sum: -5, avg: (1, 3), f64: (0.1, 3), key: 100 },
    ]
}

/// The owning node's finalized cells of one record through the aggregate library (tier 0's
/// `AggPlan::finalize`: the reference `eval` and the incumbent read).
fn rec_cells(m: Mcx<'_>, r: &RecRow, region: &mut Region) -> Vec<NullableDatum> {
    let plan = |fnoid: Oid, arg: Oid, typ: Oid| Agg::compile(aggref_tree(m, fnoid, 0, &[arg], typ).as_aggref().unwrap(), &PgCatalog).unwrap();
    let st = |v: i64, hi: i64, n: i64| AggState { value: NullableDatum::value(Datum::from_i64(v)), count: n, hi };
    let sum_ok = i64::try_from(r.sum).is_ok();
    vec![
        NullableDatum::value(Datum::from_i64(r.count)),
        if r.count == 0 || !sum_ok { NullableDatum::null() } else { NullableDatum::value(Datum::from_i64(r.sum as i64)) },
        plan(2100, INT8OID, NUMERICOID).finalize(&st(r.avg.0 as i64, (r.avg.0 >> 64) as i64, r.avg.1), region).unwrap(),
        plan(2105, FLOAT8OID, FLOAT8OID).finalize(&st(r.f64.0.to_bits() as i64, 0, r.f64.1), region).unwrap(),
        plan(2107, INT8OID, NUMERICOID).finalize(&st(r.sum as i64, (r.sum >> 64) as i64, r.count), region).unwrap(),
    ]
}

fn aggref_tree<'m>(m: Mcx<'m>, aggfnoid: Oid, aggno: i32, args: &[Oid], aggtype: Oid) -> Node<'m> {
    let mut a = Node::build::<types_nodes::primnodes::Aggref>(m).unwrap();
    a.aggfnoid = aggfnoid;
    a.aggtype = aggtype;
    a.aggkind = b'n' as i8;
    a.aggno = aggno;
    a.aggtransno = aggno;
    for &t in args {
        a.aggargtypes.lappend(m, t).unwrap();
    }
    a.seal()
}

#[repr(C)]
struct RecFrame {
    recs: *const u8,
    nrecs: u64,
    consts: *const u64,
    consts_null: *const u8,
    region: *mut Region,
    out_val: *mut u64,
    out_valid: *mut u8,
    out_err: *mut u64,
}

/// The function: the records in a loop, each the emit's row (`ColsRef::Record` for the key, the
/// cells for the `Aggref`s), the three outputs stored per record.
fn rec_build(c: &Compiled<'_>, nconsts: usize) -> Result<(gravity_ir::Program, gravity_ir::Func), EmitError> {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("aggref_test", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let recs = b.load(Type::Ptr, ctx, 0);
    let nrecs = b.load(Type::I64, ctx, 8);
    let cp = b.load(Type::Ptr, ctx, 16);
    let cnp = b.load(Type::Ptr, ctx, 24);
    let region = b.load(Type::Ptr, ctx, 32);
    let out_val = b.load(Type::Ptr, ctx, 40);
    let out_valid = b.load(Type::Ptr, ctx, 48);
    let out_err = b.load(Type::Ptr, ctx, 56);
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let mut consts = Vec::new();
    let mut consts_null = Vec::new();
    for j in 0..nconsts {
        consts.push(b.load(Type::I64, cp, (j * 8) as i32));
        let n = b.load_zext(Type::I8, cnp, j as i32);
        consts_null.push(b.icmp(IntCond::Ne, n, zero));
    }
    let l_head = b.create_block(&[Type::I64]);
    let l_row = b.create_block(&[]);
    let l_out = b.create_block(&[]);
    b.mark_loop_header(l_head);
    b.br(l_head, &[zero]);
    b.switch_to(l_head);
    let i = b.block_params(l_head)[0];
    let done = b.icmp(IntCond::Uge, i, nrecs);
    b.cond_br(done, (l_out, &[]), (l_row, &[]));
    b.switch_to(l_row);
    let rec = b.ptr_add(recs, i, R_SIZE as u8, 0);
    let row = RowRefs::record(ColsRef::Record { base: rec, offsets: &[R_KEY] }, &consts, &consts_null, rec, &R_CELLS, Some(region), scratch());
    let l = c.emit(&mut b, &row)?;
    let ov = b.ptr_add(out_val, i, 8, 0);
    b.store(ov, 0, l.value);
    let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
    let ovp = b.ptr_add(out_valid, i, 1, 0);
    b.store(ovp, 0, valid8);
    let oep = b.ptr_add(out_err, i, 8, 0);
    b.store(oep, 0, l.err);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_head, &[i1]);
    b.switch_to(l_out);
    b.ret(&[zero]);
    drop(b);
    Ok((pb.finish(), f))
}

struct RecCase {
    name: &'static str,
    typ: Oid,
    build: Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>,
}

fn rc(name: &'static str, typ: Oid, build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> RecCase {
    RecCase { name, typ, build: Box::new(build) }
}

fn rec_cases() -> Vec<RecCase> {
    fn count(m: Mcx<'_>) -> Node<'_> {
        aggref_tree(m, 2803, 0, &[], INT8OID)
    }
    fn sum(m: Mcx<'_>) -> Node<'_> {
        aggref_tree(m, 2108, 1, &[INT4OID], INT8OID)
    }
    fn avg(m: Mcx<'_>) -> Node<'_> {
        aggref_tree(m, 2100, 2, &[INT8OID], NUMERICOID)
    }
    vec![
        rc("count", INT8OID, count),
        rc("sum as int8", INT8OID, sum),
        rc("avg numeric", NUMERICOID, avg),
        rc("avg float8", FLOAT8OID, |m| aggref_tree(m, 2105, 3, &[FLOAT8OID], FLOAT8OID)),
        rc("sum int8 numeric", NUMERICOID, |m| aggref_tree(m, 2107, 4, &[INT8OID], NUMERICOID)),
        rc("having count > 100000", BOOLOID, move |m| opexpr(m, 470, BOOLOID, 0, &[count(m), int8(m, Some(100_000))])),
        rc("sum + 0 * count", INT8OID, move |m| opexpr(m, 463, INT8OID, 0, &[sum(m), opexpr(m, 465, INT8OID, 0, &[int8(m, Some(0)), count(m)])])),
        rc("sum + 3 * count", INT8OID, move |m| opexpr(m, 463, INT8OID, 0, &[sum(m), opexpr(m, 465, INT8OID, 0, &[int8(m, Some(3)), count(m)])])),
        rc("key", INT8OID, |m| scan_var(m, 1, INT8OID)),
        rc("key + count", INT8OID, move |m| opexpr(m, 463, INT8OID, 0, &[scan_var(m, 1, INT8OID), count(m)])),
        rc("case count > 2 then avg", NUMERICOID, move |m| case(m, None, &[when(m, opexpr(m, 470, BOOLOID, 0, &[count(m), int8(m, Some(2))]), avg(m))], Some(konst(m, NUMERICOID, -1, false, None)))),
        rc("coalesce(sum, -1)", INT8OID, move |m| coalesce(m, &[sum(m), int8(m, Some(-1))])),
        rc("avg is null", BOOLOID, move |m| null_test(m, avg(m), NullTestType::IS_NULL)),
        rc("sum is not null", BOOLOID, move |m| null_test(m, sum(m), NullTestType::IS_NOT_NULL)),
    ]
}

fn rec_run(c: &RecCase, rows: &[RecRow]) -> (Vec<Outcome>, Vec<Outcome>, Vec<Outcome>, Vec<Outcome>) {
    install_seams();
    let cx = MemoryContext::new("aggref-test");
    let mcx = cx.mcx();
    let expr = (c.build)(mcx);
    let mut cbuf = vec![0u8; 1 << 16];
    let mut cregion = Region::over(&mut cbuf);
    let cells: Vec<Vec<NullableDatum>> = rows.iter().map(|r| rec_cells(mcx, r, &mut cregion)).collect();
    let mut incumbent = Vec::new();
    let mut eval = Vec::new();
    let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{}: compile: {e}", c.name));
    let mut ebuf = vec![0u8; 1 << 16];
    let mut eregion = Region::over(&mut ebuf);
    for (r, cell) in rows.iter().zip(&cells) {
        let mut av: Vec<Datum> = cell.iter().map(|d| d.value).collect();
        let mut an: Vec<bool> = cell.iter().map(|d| d.isnull).collect();
        let agg = AggBind { values: core::ptr::NonNull::new(av.as_mut_ptr()).unwrap(), nulls: core::ptr::NonNull::new(an.as_mut_ptr()).unwrap(), naggs: cell.len() as u32, grouping: None, win: None };
        let mut state = exec_init_expr_subplans_agg(mcx, Some(expr), ParamBind::NONE, None, Some(agg)).unwrap().unwrap();
        state.arm_result_mcx(mcx);
        let mut slot = virtual_slot(mcx, &[INT8OID], &[Some(r.key as u64)]);
        let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
        incumbent.push(match exec_eval_expr(&mut state, &mut slots) {
            Ok(v) => bytes_of(pg_value(v, c.typ), c.typ),
            Err(e) => err_of(&e),
        });
        let (values, nulls) = ([Datum::from_i64(r.key)], [false]);
        let row = Row::scan_typed(&values, &nulls, &[INT8OID], 0).with_aggs(cell);
        eval.push(match compiled.eval(&row, &mut eregion) {
            Ok(v) => bytes_of(v, c.typ),
            Err(e) => err_of(&e),
        });
    }
    let needs = compiled.param_needs(scratch()).unwrap();
    let consts: Vec<u64> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.value.as_u64(), ParamNeed::Extern { .. } | ParamNeed::Exec { .. } | ParamNeed::Deferred { .. } => 0 }).collect();
    let consts_null: Vec<u8> = needs.iter().map(|n| match n { ParamNeed::Const(d) | ParamNeed::Elem(d) => d.isnull as u8, ParamNeed::Extern { .. } | ParamNeed::Exec { .. } | ParamNeed::Deferred { .. } => 1 }).collect();
    compiled.emit_check().unwrap_or_else(|e| panic!("{}: emit_check: {e:?}", c.name));
    let (ir, f) = rec_build(&compiled, needs.len()).unwrap_or_else(|e| panic!("{}: emit: {e:?}", c.name));
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", c.name));
    let bytes: Vec<u8> = rows.iter().flat_map(|r| rec_bytes(r)).collect();
    let n = rows.len();
    let run = |code: gravity_program::Code| -> Vec<Outcome> {
        let mut out_val = vec![0u64; n];
        let mut out_valid = vec![0u8; n];
        let mut out_err = vec![0u64; n];
        let mut rbuf = vec![0u8; 1 << 16];
        let mut region = Region::over(&mut rbuf);
        let mut frame = RecFrame { recs: bytes.as_ptr(), nrecs: n as u64, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), region: &mut region, out_val: out_val.as_mut_ptr(), out_valid: out_valid.as_mut_ptr(), out_err: out_err.as_mut_ptr() };
        // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
        let r = unsafe { code.call((&mut frame as *mut RecFrame).cast()) };
        assert_eq!(r, 0);
        (0..n).map(|i| if out_err[i] != 0 { raised(out_err[i]) } else if out_valid[i] == 0 { Outcome::Null } else { bytes_of(NullableDatum { value: Datum::from_u64(out_val[i]), isnull: false }, c.typ) }).collect()
    };
    let tpde = run(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde: {}", c.name, e.message())));
    let interp = run(gravity_backend::interp(ir, f).unwrap());
    (eval, incumbent, interp, tpde)
}

/// Every cell form over the fixture records: the emitted `Aggref` (the record's raw slots, the
/// finals through `agg_final`) equals the incumbent and `eval` over the library's finalized cells.
#[test]
fn aggref_record_cells_match_eval_the_incumbent_the_interpreter_and_tpde() {
    let rows = rec_rows();
    let (mut total, mut identical) = (0, 0);
    let mut bad = Vec::new();
    for c in rec_cases() {
        let (eval, incumbent, interp, tpde) = rec_run(&c, &rows);
        for i in 0..rows.len() {
            total += 1;
            if incumbent[i] == interp[i] && incumbent[i] == tpde[i] && eval[i] == incumbent[i] {
                identical += 1;
            } else {
                bad.push(format!("{} record {i}: eval {:?} incumbent {:?} interp {:?} tpde {:?}", c.name, eval[i], incumbent[i], interp[i], tpde[i]));
            }
        }
    }
    eprintln!("aggref cells conformance: {total} cases, {identical} identical");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

/// An `i128` sum beyond `int8` is the range flag on the row (`err`), never a truncated word; the
/// final into numeric serves it whole.
#[test]
fn aggref_i128_sum_beyond_int8_is_the_range_flag() {
    let rows = vec![RecRow { count: 2, moving: false, sum: i64::MAX as i128 + 1, avg: (0, 0), f64: (0.0, 0), key: 0 }, RecRow { count: 2, moving: false, sum: i64::MIN as i128 - 1, avg: (0, 0), f64: (0.0, 0), key: 0 }, RecRow { count: 2, moving: false, sum: i64::MIN as i128, avg: (0, 0), f64: (0.0, 0), key: 0 }];
    let (_, _, interp, tpde) = rec_run(&rc("sum as int8", INT8OID, |m| aggref_tree(m, 2108, 1, &[INT4OID], INT8OID)), &rows);
    assert_eq!(interp, vec![raised(1 << gravity_abi::status::raise::INT8_RANGE), raised(1 << gravity_abi::status::raise::INT8_RANGE), Outcome::Value((i64::MIN as u64).to_ne_bytes().to_vec())]);
    assert_eq!(tpde, interp);
    let (eval, incumbent, interp, tpde) = rec_run(&rc("sum int8 numeric", NUMERICOID, |m| aggref_tree(m, 2107, 4, &[INT8OID], NUMERICOID)), &rows);
    assert!(interp.iter().all(|o| matches!(o, Outcome::Value(_))), "{interp:?}");
    assert_eq!((&eval, &incumbent, &tpde), (&interp, &interp, &interp));
}

// ---- the tier-A datetime forms (expr-compiler.md §2.10) against the fmgr functions ----

const PG_EPOCH_JDATE: i64 = 2_451_545;
const USECS_DAY: i64 = 86_400_000_000;
/// PostgreSQL's `MIN_TIMESTAMP` and `END_TIMESTAMP`.
const TS_MIN: i64 = -PG_EPOCH_JDATE * USECS_DAY;
const TS_END: i64 = 9_223_371_331_200_000_000;

/// PostgreSQL's `date2j` (the astronomical year: 0 = 1 BC).
fn date2j(y: i64, m: i64, d: i64) -> i64 {
    let (m, y) = if m > 2 { (m + 1, y + 4800) } else { (m + 13, y + 4799) };
    let century = y / 100;
    y * 365 - 32167 + y / 4 - century + century / 4 + 7834 * m / 256 + d
}

fn ts_at(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64, us: i64) -> Word {
    Some((((date2j(y, m, d) - PG_EPOCH_JDATE) * USECS_DAY) + ((h * 60 + mi) * 60 + s) * 1_000_000 + us) as u64)
}

/// The timestamp rows: the calendar's corners (the epochs, BC, the range's ends, leap days, the
/// DST instants of a fixed-offset zone, the infinities, NULL) and a pseudo-random sweep of the range.
fn ts_rows() -> Vec<Vec<Word>> {
    let mut v: Vec<Word> = vec![
        Some(0),
        Some((-1i64) as u64),
        ts_at(1970, 1, 1, 0, 0, 0, 0),
        ts_at(2013, 7, 15, 13, 45, 59, 999_999),
        ts_at(2013, 3, 10, 2, 30, 0, 0),
        ts_at(2013, 11, 3, 1, 30, 0, 0),
        ts_at(2013, 7, 14, 0, 0, 0, 0),
        ts_at(2013, 7, 14, 23, 59, 59, 999_999),
        ts_at(1, 1, 1, 0, 0, 0, 0),
        ts_at(0, 12, 31, 23, 59, 59, 999_999),
        ts_at(0, 1, 1, 0, 0, 0, 0),
        ts_at(-1, 6, 15, 12, 0, 0, 0),
        ts_at(-9, 1, 1, 0, 0, 0, 0),
        ts_at(-10, 1, 1, 0, 0, 0, 0),
        ts_at(-99, 1, 1, 0, 0, 0, 0),
        ts_at(-100, 1, 1, 0, 0, 0, 0),
        ts_at(-999, 1, 1, 0, 0, 0, 0),
        ts_at(-1000, 1, 1, 0, 0, 0, 0),
        ts_at(-4712, 1, 1, 0, 0, 0, 0),
        Some(TS_MIN as u64),
        Some((TS_MIN + 1) as u64),
        Some((TS_END - 1) as u64),
        ts_at(294276, 12, 31, 0, 0, 0, 0),
        ts_at(2000, 2, 29, 0, 0, 0, 0),
        ts_at(1900, 3, 1, 0, 0, 0, 0),
        ts_at(1900, 2, 28, 23, 59, 59, 999_999),
        ts_at(2100, 2, 28, 23, 59, 59, 999_999),
        ts_at(1999, 12, 31, 23, 59, 59, 999_999),
        ts_at(2001, 1, 1, 0, 0, 0, 0),
        ts_at(2010, 1, 1, 0, 0, 0, 0),
        ts_at(2009, 12, 31, 23, 59, 59, 999_999),
        ts_at(1969, 12, 31, 23, 59, 59, 999_999),
        ts_at(1582, 10, 15, 0, 0, 0, 0),
        ts_at(1582, 10, 4, 0, 0, 0, 0),
        Some(i64::MIN as u64),
        Some(i64::MAX as u64),
        None,
    ];
    for m in 1..=12 {
        for d in [1, 28, 29, 30, 31] {
            let next = if m == 12 { date2j(2014, 1, 1) } else { date2j(2013, m + 1, 1) };
            let last = next - date2j(2013, m, 1);
            if d <= last {
                v.push(ts_at(2013, m, d, 12, 0, 0, 0));
                v.push(ts_at(2013, m, d, 0, 0, 0, 0));
            }
        }
    }
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..1500 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let span = (TS_END as u64).wrapping_sub(TS_MIN as u64);
        v.push(Some((TS_MIN as u64).wrapping_add(x % span)));
    }
    for _ in 0..500 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // the years around 2013, microsecond-dense
        let base = ts_at(2012, 1, 1, 0, 0, 0, 0).unwrap();
        v.push(Some(base.wrapping_add(x % (3 * 366 * USECS_DAY as u64))));
    }
    v.into_iter().map(|w| vec![w]).collect()
}

const PART_UNITS: [&str; 15] = ["microseconds", "minute", "hour", "day", "month", "quarter", "year", "dow", "isodow", "doy", "decade", "century", "millennium", "MINUTE", "week"];
const TRUNC_UNITS: [&str; 11] = ["microseconds", "milliseconds", "second", "minute", "hour", "day", "month", "quarter", "year", "Hour", "week"];

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `date_part(unit, ts)` (2021) and `date_trunc(unit, ts)` (2020) over the timestamp rows, every unit
/// the forms serve and two they refuse (`week`, `epoch`: the shim), through the four evaluators.
#[test]
fn datetime_part_and_trunc_match_the_incumbent() {
    let rows = ts_rows();
    let mut cases = Vec::new();
    for u in PART_UNITS {
        cases.push(tc(leak(format!("date_part {u}")), &[TIMESTAMPOID], &rows, FLOAT8OID, move |m| funcexpr(m, 2021, FLOAT8OID, 0, &[text_const(m, u), scan_var(m, 1, TIMESTAMPOID)])));
    }
    for u in TRUNC_UNITS {
        cases.push(tc(leak(format!("date_trunc {u}")), &[TIMESTAMPOID], &rows, TIMESTAMPOID, move |m| funcexpr(m, 2020, TIMESTAMPOID, 0, &[text_const(m, u), scan_var(m, 1, TIMESTAMPOID)])));
    }
    let mut bad = Vec::new();
    let mut total = 0;
    for c in &cases {
        let r = run_case(c);
        for i in 0..c.rows.len() {
            total += 1;
            if !(r.incumbent[i] == r.interp[i] && r.incumbent[i] == r.tpde[i] && r.eval[i] == r.incumbent[i]) {
                bad.push(format!("{} row {i} {:?}: eval {:?} incumbent {:?} interp {:?} tpde {:?}", c.name, c.rows[i], r.eval[i], r.incumbent[i], r.interp[i], r.tpde[i]));
            }
        }
    }
    eprintln!("datetime part/trunc: {total} rows compared");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

/// The sqlstate in the status word (lane one-refuser): a raising row's mask names the incumbent's
/// sqlstate and message, byte for byte, through every evaluator.
#[test]
fn raises_name_the_incumbent_error() {
    let ints: Vec<Vec<Word>> = vec![vec![i4(i32::MAX), i4(0)], vec![i4(7), i4(2)]];
    let ts: Vec<Vec<Word>> = vec![vec![Some((-211_813_488_000_000_000i64) as u64)], vec![ts_at(2013, 7, 15, 13, 45, 59, 0)]];
    let err = |s: &str, m: &str| Outcome::Error(Some((s.to_string(), m.to_string())));
    let cases = [
        (tc("int4pl overflow", &[INT4OID, INT4OID], &ints, INT4OID, |m| opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))])), err("22003", "integer out of range")),
        (tc("int4div by zero", &[INT4OID, INT4OID], &ints, INT4OID, |m| opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])), err("22012", "division by zero")),
        (tc("date_trunc year below the range", &[TIMESTAMPOID], &ts, TIMESTAMPOID, |m| funcexpr(m, 2020, TIMESTAMPOID, 0, &[text_const(m, "year"), scan_var(m, 1, TIMESTAMPOID)])), err("22008", "timestamp out of range")),
    ];
    for (c, want) in &cases {
        let r = run_case(c);
        for (who, got) in [("incumbent", &r.incumbent), ("eval", &r.eval), ("interp", &r.interp), ("tpde", &r.tpde)] {
            match (&got[0], want) {
                (Outcome::Error(Some(a)), Outcome::Error(Some(b))) => assert_eq!(a, b, "{} {who}", c.name),
                (g, _) => panic!("{} {who}: row 0 did not raise a described error: {g:?}", c.name),
            }
            assert!(matches!(got[1], Outcome::Value(_)), "{} {who}: row 1 {:?}", c.name, got[1]);
        }
    }
}

/// D3149: the key word of an `extract`'s numeric: its integer, or (a monotonic unit's field of an infinite source)
/// ±Infinity as gravity_expr's `WORD_PINF` / `WORD_NINF`.
fn extract_word(b: &[u8]) -> Option<i64> {
    use gravity_expr::emit::prim::datetime::{WORD_NINF, WORD_PINF};
    match u16::from_ne_bytes([b[0], b[1]]) {
        0xD000 => Some(WORD_PINF),
        0xF000 => Some(WORD_NINF),
        _ => numeric_word(b),
    }
}

/// The integer of a numeric image at scale 0 (`NumericData`: the short and long headers, base-10000
/// digits); `None` for NaN / ±Infinity.
fn numeric_word(b: &[u8]) -> Option<i64> {
    let u16_at = |i: usize| u16::from_ne_bytes([b[i], b[i + 1]]);
    let h = u16_at(0);
    if h & 0xC000 == 0xC000 {
        return None;
    }
    let (neg, weight, digits) = if h & 0x8000 != 0 {
        let w = (h & 0x3F) as i64;
        (h & 0x2000 != 0, if h & 0x40 != 0 { w - 0x40 } else { w }, &b[2..])
    } else {
        (h & 0x4000 != 0, u16_at(2) as i16 as i64, &b[4..])
    };
    let n = digits.len() / 2;
    let mut v: i64 = 0;
    for i in 0..n {
        v = v * 10000 + u16::from_ne_bytes([digits[2 * i], digits[2 * i + 1]]) as i64;
    }
    for _ in n as i64..=weight {
        v *= 10000;
    }
    Some(if neg { -v } else { v })
}

/// `extract(unit FROM ts)` (6202) in the key form: the emitted integer word against the incumbent's
/// numeric (NULL has no word: the emitted validity is false there; D3149: ±Infinity, a monotonic unit's field of an
/// infinite timestamp, is `WORD_PINF` / `WORD_NINF`).
#[test]
fn extract_word_form_matches_the_incumbent() {
    let rows = ts_rows();
    let mut bad = Vec::new();
    let mut total = 0;
    for u in PART_UNITS.into_iter().filter(|u| *u != "week") {
        let mut c = tc(leak(format!("extract {u}")), &[TIMESTAMPOID], &rows, NUMERICOID, move |m| funcexpr(m, 6202, NUMERICOID, 0, &[text_const(m, u), scan_var(m, 1, TIMESTAMPOID)]));
        c.word_root = true;
        let r = run_case(&c);
        for i in 0..c.rows.len() {
            total += 1;
            let expected = match &r.incumbent[i] {
                Outcome::Value(b) => extract_word(b).map_or(Outcome::Null, |w| Outcome::Value((w as u64).to_ne_bytes().to_vec())),
                o => o.clone(),
            };
            if !(expected == r.interp[i] && expected == r.tpde[i]) {
                bad.push(format!("{} row {i} {:?}: incumbent {:?} expected {:?} interp {:?} tpde {:?}", c.name, c.rows[i], r.incumbent[i], expected, r.interp[i], r.tpde[i]));
            }
        }
    }
    eprintln!("extract word form: {total} rows compared");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

/// B2-31: `extract(unit FROM d)` of a date (6199 `extract_date`) in the key form: the emitted integer word against the
/// incumbent's numeric over the date's whole range (4714-11-24 BC .. 5874897 AD, past any timestamp), the BC / AD seam,
/// leap days, the infinities (NULL: invalid; D3149: a monotonic unit's ±Infinity, its sentinel word) and NULL; the
/// time-of-day units are not taken (PostgreSQL raises there).
#[test]
fn extract_date_word_form_matches_the_incumbent() {
    const DATE_MIN: i32 = -2_451_545; // Julian day 0, 4714-11-24 BC
    const DATE_MAX: i32 = 2_147_483_494 - 2_451_545; // 5874897-12-31
    let mut days: Vec<Word> = vec![i4(DATE_MIN), i4(DATE_MIN + 1), i4(DATE_MAX), i4(DATE_MAX - 1), i4(0), i4(-1), i4(1), i4(i32::MIN), i4(i32::MAX), None];
    // around 1 BC / 1 AD (Julian days 1721058 .. 1721792), around 2000-02-29, 1900, 2100, and the timestamp range's end
    for c in [1_721_426 - 2_451_545, 0, -36_524, 36_525, 109_203_489 - 2_451_545] {
        for o in -400..400 {
            days.push(i4(c + o));
        }
    }
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..2000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        days.push(i4((DATE_MIN as i64 + (x % (DATE_MAX as i64 - DATE_MIN as i64 + 1) as u64) as i64) as i32));
    }
    let rows: Vec<Vec<Word>> = days.into_iter().map(|w| vec![w]).collect();
    let mut bad = Vec::new();
    let mut total = 0;
    for u in ["day", "month", "quarter", "year", "decade", "century", "millennium", "dow", "isodow", "doy", "YEAR"] {
        let mut c = tc(leak(format!("extract {u} of a date")), &[DATEOID], &rows, NUMERICOID, move |m| funcexpr(m, 6199, NUMERICOID, 0, &[text_const(m, u), scan_var(m, 1, DATEOID)]));
        c.word_root = true;
        let r = run_case(&c);
        for i in 0..c.rows.len() {
            total += 1;
            let expected = match &r.incumbent[i] {
                Outcome::Value(b) => extract_word(b).map_or(Outcome::Null, |w| Outcome::Value((w as u64).to_ne_bytes().to_vec())),
                o => o.clone(),
            };
            if !(expected == r.interp[i] && expected == r.tpde[i]) {
                bad.push(format!("{} row {i} {:?}: incumbent {:?} expected {:?} interp {:?} tpde {:?}", c.name, c.rows[i], r.incumbent[i], expected, r.interp[i], r.tpde[i]));
            }
        }
    }
    eprintln!("extract date word form: {total} rows compared");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

// ---- L5-SHIM: the direct by-value road and the fused datetime compares ----

/// A numeric constant `v / 10^scale` (an integer at scale 0) as a plan Const over a leaked image.
fn numeric_const(mcx: Mcx<'_>, v: i64, scale: i32) -> Node<'_> {
    let img = if scale == 0 { adt_numeric::int64_to_numeric(v) } else { adt_numeric::int64_div_fast_to_numeric(v, scale).unwrap() };
    let bytes: &'static [u8] = Box::leak(img.as_bytes().to_vec().into_boxed_slice());
    konst(mcx, NUMERICOID, -1, false, Some(Datum::from_usize(bytes.as_ptr() as usize)))
}

/// An interval constant `{time, day, month}` as a plan Const over a leaked 16-byte image.
fn interval_const(mcx: Mcx<'_>, time: i64, day: i32, month: i32) -> Node<'_> {
    let mut img = [0u8; 16];
    img[0..8].copy_from_slice(&time.to_ne_bytes());
    img[8..12].copy_from_slice(&day.to_ne_bytes());
    img[12..16].copy_from_slice(&month.to_ne_bytes());
    let b: &'static mut [u64; 2] = Box::leak(Box::new([0u64; 2]));
    // SAFETY: the two words are 16 bytes, 8-aligned, leaked for the process.
    unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), b.as_mut_ptr().cast::<u8>(), 16) };
    konst(mcx, INTERVALOID, 16, false, Some(Datum::from_usize(b.as_ptr() as usize)))
}

fn extract_ts<'m>(m: Mcx<'m>, u: &'static str) -> Node<'m> {
    funcexpr(m, 6202, NUMERICOID, 0, &[text_const(m, u), scan_var(m, 1, TIMESTAMPOID)])
}

fn check_cases(what: &str, cases: &[Case]) {
    let mut bad = Vec::new();
    let mut total = 0;
    for c in cases {
        let r = run_case(c);
        for i in 0..c.rows.len() {
            total += 1;
            if !(r.incumbent[i] == r.interp[i] && r.incumbent[i] == r.tpde[i] && r.eval[i] == r.incumbent[i]) {
                bad.push(format!("{} row {i} {:?}: eval {:?} incumbent {:?} interp {:?} tpde {:?}", c.name, c.rows[i], r.eval[i], r.incumbent[i], r.interp[i], r.tpde[i]));
            }
        }
    }
    eprintln!("{what}: {total} rows compared");
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}

/// The direct road (host.rs `direct`): by-value shims called with their words in registers — `gcd` (F1's), `make_date`
/// (F3's), `sqrt`, `int8gcd` — over the values, the NULLs and the raising rows, through the four evaluators.
#[test]
fn direct_byval_shims_match_the_incumbent() {
    let ii: Vec<Vec<Word>> = vec![vec![i4(7), i4(3)], vec![i4(-7), i4(2)], vec![i4(i32::MIN), i4(-1)], vec![i4(i32::MIN), i4(0)], vec![i4(i32::MIN), i4(i32::MIN)], vec![i4(i32::MAX), i4(1)], vec![i4(5), i4(0)], vec![n(), i4(1)], vec![i4(1), n()], vec![n(), n()], vec![i4(0), i4(0)], vec![i4(100), i4(6)], vec![i4(2013), i4(13)], vec![i4(2013), i4(2)], vec![i4(-2013), i4(2)], vec![i4(0), i4(1)]];
    let ss: Vec<Vec<Word>> = vec![vec![i2(7), i8w(3)], vec![i2(i16::MAX), i8w(i64::MAX)], vec![i2(i16::MIN), i8w(-1)], vec![i2(-1), i8w(i64::MIN)], vec![n(), i8w(0)], vec![i2(5), i8w(0)], vec![i2(0), n()]];
    let cases = vec![
        tc("int4gcd", &[INT4OID, INT4OID], &ii, INT4OID, |m| funcexpr(m, 5044, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])),
        tc("gcd(a, 6) = 1", &[INT4OID, INT4OID], &ii, BOOLOID, |m| opexpr(m, 65, BOOLOID, 0, &[funcexpr(m, 5044, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(6))]), int4(m, Some(1))])),
        tc("make_date", &[INT4OID, INT4OID], &ii, DATEOID, |m| funcexpr(m, 3846, DATEOID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID), int4(m, Some(15))])),
        tc("make_date > date", &[INT4OID, INT4OID], &ii, BOOLOID, |m| opexpr(m, 1089, BOOLOID, 0, &[funcexpr(m, 3846, DATEOID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID), int4(m, Some(15))]), relabel(m, int4(m, Some(3803)), DATEOID)])),
        tc("dsqrt", &[INT4OID, INT4OID], &ii, FLOAT8OID, |m| funcexpr(m, 230, FLOAT8OID, 0, &[funcexpr(m, 316, FLOAT8OID, 0, &[scan_var(m, 1, INT4OID)])])),
        tc("int8gcd", &[INT2OID, INT8OID], &ss, INT8OID, |m| funcexpr(m, 5045, INT8OID, 0, &[scan_var(m, 2, INT8OID), scan_var(m, 2, INT8OID)])),
        tc("int8gcd min", &[INT2OID, INT8OID], &ss, INT8OID, |m| funcexpr(m, 5045, INT8OID, 0, &[scan_var(m, 2, INT8OID), int8(m, Some(0))])),
    ];
    check_cases("direct byval shims", &cases);
}

/// `host::NULLABLE_BYVAL` is exactly the set of `shims/byval.rs` bodies that can `null_ret`.
#[test]
fn nullable_byval_list_matches_the_generated_family() {
    let src = include_str!("../../fn/src/shims/byval.rs");
    let mut names: Vec<&str> = Vec::new();
    let mut cur = "";
    for line in src.lines() {
        if let Some(rest) = line.strip_prefix("pub extern \"C-unwind\" fn ") {
            cur = rest.split('(').next().unwrap();
        } else if line.contains("null_ret(") && !names.contains(&cur) {
            names.push(cur);
        }
    }
    let mut want: Vec<&str> = gravity_expr::emit::host::NULLABLE_BYVAL.to_vec();
    names.sort();
    want.sort();
    assert_eq!(names, want, "the by-value shims returning NULL");
}

/// `extract(unit FROM ts) ⋈ K` (T2's form) fused into the integer field's compare: every integral unit, the six
/// compares, both operand orders, over the timestamp rows (BC, the range's ends, the infinities, NULL); a fractional K,
/// a fractional unit and `week` keep the shim road and must agree too.
fn fused_extract_compare_unit(u: &'static str) {
    let rows = ts_rows();
    let mut cases = Vec::new();
    for (op, name) in [(1718u32, "="), (1719, "<>"), (1720, ">"), (1721, ">="), (1722, "<"), (1723, "<=")] {
        for k in [2013i64, 7, 0, -1] {
            cases.push(tc(leak(format!("extract {u} {name} {k}")), &[TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, op, BOOLOID, 0, &[extract_ts(m, u), numeric_const(m, k, 0)])));
            cases.push(tc(leak(format!("{k} {name} extract {u}")), &[TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, op, BOOLOID, 0, &[numeric_const(m, k, 0), extract_ts(m, u)])));
        }
    }
    cases.push(tc(leak(format!("extract {u} = 2013.5 (shim road)")), &[TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, 1718, BOOLOID, 0, &[extract_ts(m, u), numeric_const(m, 20135, 1)])));
    cases.push(tc(leak(format!("extract {u} = 2013.0 (shim road)")), &[TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, 1718, BOOLOID, 0, &[extract_ts(m, u), numeric_const(m, 20130, 1)])));
    check_cases(leak(format!("fused extract compares ({u})")), &cases);
}

// one test per PART_UNITS unit (the gate runs them in parallel: the single test was 68 s); fused_extract_compare_units_cover_part_units keeps the list whole
macro_rules! fused_extract_compare_tests {
    ($($f:ident => $u:literal),* $(,)?) => {
        $(#[test] fn $f() { fused_extract_compare_unit($u); })*
        const FUSED_EXTRACT_UNITS: &[&str] = &[$($u),*];
    };
}
fused_extract_compare_tests! {
    fused_extract_compare_microseconds_matches_the_incumbent => "microseconds", fused_extract_compare_minute_matches_the_incumbent => "minute",
    fused_extract_compare_hour_matches_the_incumbent => "hour", fused_extract_compare_day_matches_the_incumbent => "day",
    fused_extract_compare_month_matches_the_incumbent => "month", fused_extract_compare_quarter_matches_the_incumbent => "quarter",
    fused_extract_compare_year_matches_the_incumbent => "year", fused_extract_compare_dow_matches_the_incumbent => "dow",
    fused_extract_compare_isodow_matches_the_incumbent => "isodow", fused_extract_compare_doy_matches_the_incumbent => "doy",
    fused_extract_compare_decade_matches_the_incumbent => "decade", fused_extract_compare_century_matches_the_incumbent => "century",
    fused_extract_compare_millennium_matches_the_incumbent => "millennium", fused_extract_compare_upper_minute_matches_the_incumbent => "MINUTE",
    fused_extract_compare_week_matches_the_incumbent => "week",
}

#[test]
fn fused_extract_compare_units_cover_part_units() {
    assert_eq!(FUSED_EXTRACT_UNITS, &PART_UNITS[..]);
}

#[test]
fn fused_extract_compare_second_shim_matches_the_incumbent() {
    let rows = ts_rows();
    let cases = vec![tc("extract second = 59 (shim road)", &[TIMESTAMPOID], &rows, BOOLOID, |m| opexpr(m, 1718, BOOLOID, 0, &[extract_ts(m, "second"), numeric_const(m, 59, 0)]))];
    check_cases("fused extract compares (second shim)", &cases);
}

/// `(a − b) ⋈ interval K` (F7's form) fused into the difference's compare: the six compares, both operand orders,
/// the constants' spans, over pairs with the infinities (the same infinity raises), the range's ends (the difference
/// overflows: raises) and NULL; a constant whose span leaves the word keeps the shim road and must agree too.
#[test]
fn fused_timestamp_difference_compare_matches_the_incumbent() {
    let (min_ts, max_ts) = (Some((-211_813_488_000_000_000i64) as u64), ts_at(294276, 12, 31, 23, 59, 59, 999_999));
    let (ninf, pinf) = (Some(i64::MIN as u64), Some(i64::MAX as u64));
    let a = ts_at(2013, 7, 15, 12, 0, 0, 0);
    let b = ts_at(2013, 1, 1, 0, 0, 0, 0);
    let c = ts_at(2013, 4, 11, 12, 0, 0, 0);
    let rows: Vec<Vec<Word>> = vec![vec![a, b], vec![b, a], vec![a, a], vec![c, b], vec![b, c], vec![Some(0), Some((-1i64) as u64)], vec![Some(1), Some(0)], vec![pinf, a], vec![a, pinf], vec![ninf, a], vec![a, ninf], vec![pinf, pinf], vec![ninf, ninf], vec![pinf, ninf], vec![ninf, pinf], vec![max_ts, min_ts], vec![min_ts, max_ts], vec![max_ts, b], vec![min_ts, b], vec![n(), a], vec![a, n()], vec![n(), n()], vec![n(), pinf]];
    let mut cases = Vec::new();
    for (op, name) in [(1162u32, "="), (1163, "<>"), (1164, "<"), (1165, "<="), (1166, ">="), (1167, ">")] {
        for (time, day, month, label) in [(0i64, 100i32, 0i32, "100 days"), (0, -1, 0, "-1 day"), (4 * 3_600_000_000 + 5 * 60_000_000 + 6_000_000, 3, 14, "1 year 2 months 3 days 04:05:06"), (1, 0, 0, "1 us"), (0, 0, 0, "0"), (-1, 0, 0, "-1 us"), (0, 0, 178_000_000 * 12, "178000000 years (shim road)")] {
            cases.push(tc(leak(format!("(a - b) {name} '{label}'")), &[TIMESTAMPOID, TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, op, BOOLOID, 0, &[opexpr(m, 2031, INTERVALOID, 0, &[scan_var(m, 1, TIMESTAMPOID), scan_var(m, 2, TIMESTAMPOID)]), interval_const(m, time, day, month)])));
            cases.push(tc(leak(format!("'{label}' {name} (a - b)")), &[TIMESTAMPOID, TIMESTAMPOID], &rows, BOOLOID, move |m| opexpr(m, op, BOOLOID, 0, &[interval_const(m, time, day, month), opexpr(m, 2031, INTERVALOID, 0, &[scan_var(m, 1, TIMESTAMPOID), scan_var(m, 2, TIMESTAMPOID)])])));
        }
    }
    check_cases("fused timestamp difference compares", &cases);
}

// ---- GV-D1: a row with two errors raises PostgreSQL's first, in evaluation order ----

const TEXTARRAYOID: Oid = 1009;

fn int2c(mcx: Mcx<'_>, v: i16) -> Node<'_> {
    konst(mcx, INT2OID, 2, true, Some(Datum::from_i16(v)))
}

fn text_array_const<'m>(mcx: Mcx<'m>, elems: &[&str]) -> Node<'m> {
    let values: Vec<Datum> = elems.iter().map(|s| text_image(mcx, s)).collect();
    let img = arrayfuncs::construct_md_array(mcx, &values, None, 1, &[elems.len() as i32], &[1], TEXTOID, -1, false, b'i').unwrap();
    Node::mk_const(mcx, TEXTARRAYOID, -1, 950, -1, Datum::from_usize(img.leak().as_ptr() as usize), false, false).unwrap()
}

/// `s op ANY (text[])` over a hosted text operator (`~~` textlike, `~` textregexeq).
fn text_any<'m>(mcx: Mcx<'m>, opno: Oid, opfuncid: Oid, scalar: Node<'m>, elems: &[&str]) -> Node<'m> {
    let array = text_array_const(mcx, elems);
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno, opfuncid, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: 950, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

/// Per row, the error the lowerer reports (lowerv4 hir.rs `h_raise`, exec glue.rs `pg_error`): an in-IR first code by its
/// text, `HOST` by the first site cell holding an error. Each row is one call and the cells live across the rows, as a
/// program's do, so an error an unreported call left in a cell shows. `[incumbent, interp, tpde]`.
fn run_reported(c: &Case) -> [Vec<Outcome>; 3] {
    use gravity_abi::status::raise;
    install_seams();
    let cx = MemoryContext::new("emit-test reported");
    let mcx = cx.mcx();
    let expr = (c.build)(mcx);
    let incumbent: Vec<Outcome> = c
        .rows
        .iter()
        .map(|row| {
            let mut state = exec_init_expr(mcx, Some(expr), ParamBind::NONE).unwrap().unwrap();
            state.arm_result_mcx(mcx);
            let mut slot = virtual_slot(mcx, &c.types, row);
            let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
            match exec_eval_expr(&mut state, &mut slots) {
                Ok(v) => bytes_of(v, c.typ),
                Err(e) => err_of(&e),
            }
        })
        .collect();
    let mut compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{}: compile: {e}", c.name));
    let needs = compiled.param_needs(scratch()).unwrap();
    let (consts, consts_null): (Vec<u64>, Vec<u8>) = needs
        .iter()
        .map(|n| match n {
            ParamNeed::Const(d) | ParamNeed::Elem(d) => (d.value.as_u64(), d.isnull as u8),
            _ => unreachable!("{}: no parameters here", c.name),
        })
        .unzip();
    compiled.emit_check().unwrap_or_else(|e| panic!("{}: emit_check: {e:?}", c.name));
    let (ir, f) = build(&compiled, c.types.len(), needs.len(), false, false).unwrap_or_else(|e| panic!("{}: emit: {e:?}", c.name));
    gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", c.name));
    let nsites = compiled.prog.sites.len();
    let mut run = |code: gravity_program::Code| -> Vec<Outcome> {
        let mut state: EmitState = compiled.emit_state(emit_cx());
        c.rows
            .iter()
            .map(|row| {
                let values: Vec<u64> = row.iter().map(|v| v.unwrap_or(0)).collect();
                let nulls: Vec<u8> = row.iter().map(|v| v.is_none() as u8).collect();
                let (mut val, mut valid, mut err) = ([0u64], [0u8], [0u64]);
                let mut rbuf = vec![0u8; 1 << 16];
                let mut region = Region::over(&mut rbuf);
                let mut frame = Frame { values: values.as_ptr(), nulls: nulls.as_ptr(), nrows: 1, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), region: &mut region, cells: state.ptr(), out_val: val.as_mut_ptr(), out_valid: valid.as_mut_ptr(), out_err: err.as_mut_ptr(), whole_desc: core::ptr::null() };
                // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
                assert_eq!(unsafe { code.call((&mut frame as *mut Frame).cast()) }, 0);
                match err[0] {
                    0 if valid[0] == 0 => Outcome::Null,
                    0 => bytes_of(NullableDatum { value: Datum::from_u64(val[0]), isnull: false }, c.typ),
                    e if raise::first(e) == raise::HOST => (0..nsites).find_map(|s| state.take_error(s)).map_or(Outcome::Error(None), |e| err_of(&e)),
                    e => raised(e),
                }
            })
            .collect()
    };
    let tpde = run(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde: {}", c.name, e.message())));
    let interp = run(gravity_backend::interp(ir, f).unwrap());
    [incumbent, interp, tpde]
}

/// Equal outcomes, an error only when both name the same (sqlstate, message).
fn same_reported(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Error(Some(x)), Outcome::Error(Some(y))) => x == y,
        (Outcome::Error(_), _) | (_, Outcome::Error(_)) => false,
        _ => a == b,
    }
}

fn two_error_cases() -> Vec<Case> {
    // (a int4, b int4, c int2, d int2, s text, u text): `a / b` divides by zero where b = 0, `c + d` overflows int2 where
    // c = 32767, `u::int4` / `s::int4` are bad input where not digits, the LIKE pattern `\` ends with the escape
    let cx = Box::leak(Box::new(MemoryContext::new("emit-test two errors")));
    let m = cx.mcx();
    let t = |s: &str| Some(text_image(m, s).as_u64());
    let rows: Vec<Vec<Word>> = vec![
        vec![i4(1), i4(0), i2(32767), i2(1), t("a"), t("x")],
        vec![i4(1), i4(0), i2(1), i2(1), t("a"), t("7")],
        vec![i4(1), i4(1), i2(32767), i2(1), t("z"), t("x")],
        vec![i4(6), i4(3), i2(1), i2(1), t("z"), t("7")],
        vec![i4(0), i4(0), i2(32767), i2(1), t("b"), t("9")],
        vec![n(), i4(0), i2(32767), i2(1), n(), n()],
    ];
    let ty = [INT4OID, INT4OID, INT2OID, INT2OID, TEXTOID, TEXTOID];
    fn div(m: Mcx<'_>) -> Node<'_> {
        opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])
    }
    fn pl2(m: Mcx<'_>) -> Node<'_> {
        opexpr(m, 176, INT2OID, 0, &[scan_var(m, 3, INT2OID), scan_var(m, 4, INT2OID)])
    }
    fn u4(m: Mcx<'_>) -> Node<'_> {
        coerce_via_io(m, scan_var(m, 6, TEXTOID), INT4OID)
    }
    fn s4(m: Mcx<'_>) -> Node<'_> {
        coerce_via_io(m, scan_var(m, 5, TEXTOID), INT4OID)
    }
    vec![
        // the register's reproduction, both orders: int42pl(int4div, int2pl), int24pl(int2pl, int4div)
        tc("(a / b) + (c + d): the division first", &ty, &rows, INT4OID, |m| opexpr(m, 179, INT4OID, 0, &[div(m), pl2(m)])),
        tc("(c + d) + (a / b): the overflow first", &ty, &rows, INT4OID, move |m| opexpr(m, 178, INT4OID, 0, &[pl2(m), div(m)])),
        // a nested argument's raise before the outer function's own (the outer sees the masked quotient)
        tc("(a / b) + 2147483647: the nested division first", &ty, &rows, INT4OID, move |m| opexpr(m, 177, INT4OID, 0, &[div(m), int4(m, Some(i32::MAX))])),
        tc("(a / b)::int2 + 32767: the nested division before the outer int2 overflow", &ty, &rows, INT2OID, move |m| opexpr(m, 176, INT2OID, 0, &[funcexpr(m, 314, INT2OID, 0, &[div(m)]), int2c(m, i16::MAX)])),
        // a hosted call (tier C int4in) after an in-IR raise never runs; before one it raises first
        tc("(a / b) + u::int4: the division before the shim", &ty, &rows, INT4OID, move |m| opexpr(m, 177, INT4OID, 0, &[div(m), u4(m)])),
        tc("u::int4 + (a / b): the shim before the division", &ty, &rows, INT4OID, move |m| opexpr(m, 177, INT4OID, 0, &[u4(m), div(m)])),
        // short-circuit nodes report only what PG evaluates
        tc("CASE WHEN b = 0 THEN (c + d)::int4 ELSE a / b END", &ty, &rows, INT4OID, move |m| case(m, None, &[when(m, opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), funcexpr(m, 313, INT4OID, 0, &[pl2(m)]))], Some(div(m)))),
        tc("COALESCE((c + d)::int4, a / b)", &ty, &rows, INT4OID, move |m| coalesce(m, &[funcexpr(m, 313, INT4OID, 0, &[pl2(m)]), div(m)])),
        tc("b <> 0 AND (a / b) + (c + d) > 0", &ty, &rows, BOOLOID, move |m| boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 144, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(0))]), opexpr(m, 147, BOOLOID, 0, &[opexpr(m, 179, INT4OID, 0, &[div(m), pl2(m)]), int4(m, Some(0))])])),
        // an unrolled hosted ANY stops at its first hit: the later element's LIKE error never runs, so the row's raise
        // (u::int4's) is the one reported, not the unreported element's left in its cell
        tc("s LIKE ANY ('{a%, \\}') AND u::int4 > 0", &ty, &rows, BOOLOID, move |m| boolexpr(m, BoolExprType::AND_EXPR, &[text_any(m, 1209, 850, scan_var(m, 5, TEXTOID), &["a%", "\\"]), opexpr(m, 147, BOOLOID, 0, &[u4(m), int4(m, Some(0))])])),
        // a RowCompare stops at the pair that decides: pair 2's u::int4 never runs, s::int4's error is the row's
        tc("ROW(a, u::int4) < ROW(0, 5) OR s::int4 > 0", &ty, &rows, BOOLOID, move |m| boolexpr(m, BoolExprType::OR_EXPR, &[row_compare(m, 1, &[scan_var(m, 1, INT4OID), u4(m)], &[int4(m, Some(0)), int4(m, Some(5))]), opexpr(m, 147, BOOLOID, 0, &[s4(m), int4(m, Some(0))])])),
    ]
}

#[test]
fn two_errors_raise_the_first_in_evaluation_order() {
    let mut bad = Vec::new();
    for c in two_error_cases() {
        let [inc, interp, tpde] = run_reported(&c);
        for i in 0..c.rows.len() {
            if !(same_reported(&inc[i], &interp[i]) && same_reported(&inc[i], &tpde[i])) {
                bad.push(format!("{} row {i}: incumbent {:?} interp {:?} tpde {:?}", c.name, inc[i], interp[i], tpde[i]));
            }
        }
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn a_hosted_any_reports_its_first_raising_element() {
    let cx = Box::leak(Box::new(MemoryContext::new("emit-test regex")));
    let m = cx.mcx();
    let rows: Vec<Vec<Word>> = vec![vec![Some(text_image(m, "a").as_u64())], vec![Some(text_image(m, "(").as_u64())], vec![n()]];
    let cases = vec![
        tc("s ~ ANY ('{(, [}'): the first element's error", &[TEXTOID], &rows, BOOLOID, |m| text_any(m, 641, 1254, scan_var(m, 1, TEXTOID), &["(", "["])),
        tc("s ~ ANY ('{a, (}'): the first hit stops it", &[TEXTOID], &rows, BOOLOID, |m| text_any(m, 641, 1254, scan_var(m, 1, TEXTOID), &["a", "("])),
    ];
    let mut bad = Vec::new();
    for c in cases {
        let [inc, interp, tpde] = run_reported(&c);
        for i in 0..c.rows.len() {
            if !(same_reported(&inc[i], &interp[i]) && same_reported(&inc[i], &tpde[i])) {
                bad.push(format!("{} row {i}: incumbent {:?} interp {:?} tpde {:?}", c.name, inc[i], interp[i], tpde[i]));
            }
        }
    }
    assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
}

/// `RowRefs::col_raises` (lowerv4's claimed sites): the frame's four words `a, b, k, e` (columns 1..3; `e` the raise
/// word column 3's read carries), then the expression's constants; the raise word out.
#[repr(C)]
struct RaiseFrame {
    words: [u64; 4],
    consts: *const u64,
    consts_null: *const u8,
    out_err: u64,
}

#[test]
fn a_column_raise_word_takes_its_place_in_the_order() {
    use gravity_abi::status::raise;
    install_seams();
    let cx = MemoryContext::new("emit-test col raises");
    let m = cx.mcx();
    let (a, b, k) = (scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID), scan_var(m, 3, INT4OID));
    let div = opexpr(m, 154, INT4OID, 0, &[a, b]);
    let (int2, dz) = (raise::bit(raise::INT2_RANGE), raise::bit(raise::DIV_ZERO));
    // (expression, [(a, b, raise word of k), expected raise word])
    let cases: Vec<(&str, Node<'_>, Vec<([u64; 3], u64)>)> = vec![
        ("(a / b) + k", opexpr(m, 177, INT4OID, 0, &[div, k]), vec![([1, 0, int2], dz), ([1, 1, int2], int2), ([1, 0, 0], dz), ([1, 1, 0], 0)]),
        ("k + (a / b)", opexpr(m, 177, INT4OID, 0, &[k, div]), vec![([1, 0, int2], int2), ([1, 1, int2], int2), ([1, 0, 0], dz)]),
        ("CASE WHEN b = 0 THEN 0 ELSE k END", case(m, None, &[when(m, opexpr(m, 65, BOOLOID, 0, &[b, int4(m, Some(0))]), int4(m, Some(0)))], Some(k)), vec![([1, 0, int2], 0), ([1, 1, int2], int2)]),
        ("b <> 0 AND k > 0", boolexpr(m, BoolExprType::AND_EXPR, &[opexpr(m, 144, BOOLOID, 0, &[b, int4(m, Some(0))]), opexpr(m, 147, BOOLOID, 0, &[k, int4(m, Some(0))])]), vec![([1, 0, int2], 0), ([1, 1, int2], int2)]),
    ];
    for (name, expr, rows) in cases {
        let compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: compile: {e}"));
        let needs = compiled.param_needs(scratch()).unwrap();
        let consts: Vec<u64> = needs.iter().map(|n| if let ParamNeed::Const(d) = n { d.value.as_u64() } else { unreachable!() }).collect();
        let consts_null = vec![0u8; consts.len()];
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("col_raises", &[Type::Ptr], &[Type::I64]);
        let mut fb = pb.define(f);
        let ctx = fb.params()[0];
        let w: Vec<Value> = (0..4).map(|i| fb.load(Type::I64, ctx, i * 8)).collect();
        let (cp, cnp) = (fb.load(Type::Ptr, ctx, 32), fb.load(Type::Ptr, ctx, 40));
        let zero = fb.iconst(Type::I64, 0);
        let (cv, cn): (Vec<Value>, Vec<Value>) = (0..consts.len())
            .map(|j| {
                let v = fb.load(Type::I64, cp, (j * 8) as i32);
                let nb = fb.load_zext(Type::I8, cnp, j as i32);
                (v, fb.icmp(IntCond::Ne, nb, zero))
            })
            .unzip();
        let t = fb.bconst(true);
        let (values, valids, raises) = ([w[0], w[1], w[2]], [t, t, t], [(3i16, w[3])]);
        let cols = ColsRef::Values { values: &values, valids: &valids };
        let row = RowRefs { col_raises: &raises, ..RowRefs::scan(cols, &cv, &cn, scratch()) };
        let l = compiled.emit(&mut fb, &row).unwrap_or_else(|e| panic!("{name}: emit: {e:?}"));
        fb.store(ctx, 48, l.err);
        fb.ret(&[zero]);
        drop(fb);
        let ir = pb.finish();
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name}: verify: {e:?}"));
        for (label, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap()), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
            for &([a, b, e], want) in &rows {
                let mut frame = RaiseFrame { words: [Datum::from_i32(a as i32).as_u64(), Datum::from_i32(b as i32).as_u64(), Datum::from_i32(5).as_u64(), e], consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), out_err: u64::MAX };
                // SAFETY: the frame is the ctx the function reads; its pointers are live for the call.
                assert_eq!(unsafe { code.call((&mut frame as *mut RaiseFrame).cast()) }, 0);
                assert_eq!(frame.out_err, want, "{name} ({label}) a={a} b={b} k's raise {e:#x}");
            }
        }
    }
}

/// D857: `((a + b) * b) * c` over int4, with b = MIN and c = 2: the narrow division's dividend word is i64::MIN when a is
/// NULL (its word 0 flows through the tier-A forms, which run before the call knows its arguments) or when `b * b`
/// raised (its word flows on). `/ -1` must not run `sdiv i64::MIN, -1` (the interpreter traps; x86-64 TPDE is #DE, the
/// backend dies): PG returns NULL for the NULL row and 22003 for the rest.
fn d857_i4<'m>(m: Mcx<'m>, f: Oid, with_a: bool) -> Node<'m> {
    let b = || scan_var(m, 2, INT4OID);
    let s = if with_a { opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), b()]) } else { b() };
    let p = opexpr(m, 141, INT4OID, 0, &[s, b()]);
    let p = opexpr(m, 141, INT4OID, 0, &[p, scan_var(m, 3, INT4OID)]);
    opexpr(m, f, INT4OID, 0, &[p, scan_var(m, 4, INT4OID)])
}

/// D857's int2 shape: `((a + b) * b) * ((a + b) * b) * c` with b = -32768 and c = 8 wraps the word to i64::MIN.
fn d857_i2<'m>(m: Mcx<'m>, f: Oid) -> Node<'m> {
    let sq = || opexpr(m, 152, INT2OID, 0, &[opexpr(m, 176, INT2OID, 0, &[scan_var(m, 1, INT2OID), scan_var(m, 2, INT2OID)]), scan_var(m, 2, INT2OID)]);
    let p = opexpr(m, 152, INT2OID, 0, &[sq(), sq()]);
    let p = opexpr(m, 152, INT2OID, 0, &[p, scan_var(m, 3, INT2OID)]);
    opexpr(m, f, INT2OID, 0, &[p, scan_var(m, 4, INT2OID)])
}

#[test]
fn d857_narrow_division_of_a_min_word_by_minus_one() {
    let r4: Vec<Vec<Word>> = vec![
        vec![n(), i4(i32::MIN), i4(2), i4(-1)],
        vec![i4(1), i4(i32::MIN), i4(2), i4(-1)],
        vec![i4(0), i4(i32::MIN), i4(1), i4(-1)],
        vec![n(), i4(i32::MIN), i4(2), i4(0)],
        vec![i4(7), i4(3), i4(1), i4(-1)],
    ];
    let r2: Vec<Vec<Word>> = vec![vec![n(), i2(i16::MIN), i2(8), i2(-1)], vec![i2(0), i2(i16::MIN), i2(8), i2(-1)], vec![n(), i2(i16::MIN), i2(8), i2(0)], vec![i2(7), i2(3), i2(1), i2(-1)]];
    let cases = vec![
        tc("int4div of a NULL row's MIN word by -1", &[INT4OID; 4], &r4, INT4OID, |m| d857_i4(m, 154, true)),
        tc("int4div of an erroring product's MIN word by -1", &[INT4OID; 4], &r4, INT4OID, |m| d857_i4(m, 154, false)),
        tc("int4mod of a NULL row's MIN word by -1", &[INT4OID; 4], &r4, INT4OID, |m| d857_i4(m, 156, true)),
        tc("int2div of a NULL row's MIN word by -1", &[INT2OID; 4], &r2, INT2OID, |m| d857_i2(m, 153)),
        tc("int2mod of a NULL row's MIN word by -1", &[INT2OID; 4], &r2, INT2OID, |m| d857_i2(m, 155)),
    ];
    check_cases("D857 narrow division", &cases);
}

/// D858: `o::int4` is a binary-coercible RelabelType (no code), but an oid word is zero-extended (`ObjectIdGetDatum`)
/// while the open-coded int4 forms read the whole word: over o = 4294967295 `o::int4 < 0` and `= -1` read false and
/// `+ 0` raised 22003, where PG's `DatumGetInt32` reads -1. The relabel re-forms the word (and int4 -> oid zero-extends).
fn d858_o<'m>(m: Mcx<'m>) -> Node<'m> {
    relabel(m, scan_var(m, 1, OIDOID), INT4OID)
}

#[test]
fn d858_an_oid_relabelled_to_int4_reads_its_low_32_bits_signed() {
    let rows: Vec<Vec<Word>> = vec![vec![Some(4_294_967_295), i4(1)], vec![Some(2_147_483_648), i4(-1)], vec![Some(2_147_483_647), i4(-2_147_483_648)], vec![Some(0), i4(0)], vec![n(), i4(5)], vec![Some(7), n()]];
    let t = &[OIDOID, INT4OID];
    let cases = vec![
        // no bare `o::int4` root: the incumbent returns the oid's word untouched (C's relabel is no code), Gravity its int4
        // word (sign-extended, as Int32GetDatum); both read the same through DatumGetInt32, which every case below checks
        tc("o::int4 < 0", t, &rows, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[d858_o(m), int4(m, Some(0))])),
        tc("o::int4 = -1", t, &rows, BOOLOID, |m| opexpr(m, 65, BOOLOID, 0, &[d858_o(m), int4(m, Some(-1))])),
        tc("o::int4 = i", t, &rows, BOOLOID, |m| opexpr(m, 65, BOOLOID, 0, &[d858_o(m), scan_var(m, 2, INT4OID)])),
        tc("o::int4 + 0", t, &rows, INT4OID, |m| opexpr(m, 177, INT4OID, 0, &[d858_o(m), int4(m, Some(0))])),
        tc("o::int4 * 2", t, &rows, INT4OID, |m| opexpr(m, 141, INT4OID, 0, &[d858_o(m), int4(m, Some(2))])),
        tc("o::int4 / -1", t, &rows, INT4OID, |m| opexpr(m, 154, INT4OID, 0, &[d858_o(m), int4(m, Some(-1))])),
        tc("o::int4::int8", t, &rows, INT8OID, |m| funcexpr(m, 481, INT8OID, 0, &[d858_o(m)])),
        tc("o::int4::float8", t, &rows, FLOAT8OID, |m| funcexpr(m, 316, FLOAT8OID, 0, &[d858_o(m)])),
        tc("btint4cmp(o::int4, i)", t, &rows, INT4OID, |m| funcexpr(m, 351, INT4OID, 0, &[d858_o(m), scan_var(m, 2, INT4OID)])),
        tc("i::oid = o", t, &rows, BOOLOID, |m| opexpr(m, 184, BOOLOID, 0, &[relabel(m, scan_var(m, 2, INT4OID), OIDOID), scan_var(m, 1, OIDOID)])),
        tc("o::int4::oid = o", t, &rows, BOOLOID, |m| opexpr(m, 184, BOOLOID, 0, &[relabel(m, d858_o(m), OIDOID), scan_var(m, 1, OIDOID)])),
        Case { extern_params: vec![(OIDOID, Some(4_294_967_295))], ..tc("$1::int4 < 0 (an oid parameter)", t, &rows, BOOLOID, |m| opexpr(m, 66, BOOLOID, 0, &[relabel(m, param(m, 1, OIDOID), INT4OID), int4(m, Some(0))])) },
        Case { extern_params: vec![(OIDOID, Some(4_294_967_295))], ..tc("$1::int4 = i (an oid parameter)", t, &rows, BOOLOID, |m| opexpr(m, 65, BOOLOID, 0, &[relabel(m, param(m, 1, OIDOID), INT4OID), scan_var(m, 2, INT4OID)])) },
    ];
    check_cases("D858 oid relabelled to int4", &cases);
}

/// `scalar op ANY / ALL (array)` over operator `opno` (function `opfuncid`, collation `coll`).
fn saop_by<'m>(mcx: Mcx<'m>, opno: Oid, opfuncid: Oid, coll: Oid, use_or: bool, scalar: Node<'m>, array: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno, opfuncid, hashfuncid: 0, negfuncid: 0, useOr: use_or, inputcollid: coll, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

/// A one-dimension array image of `elem` (length `len`, by value or not) in `mcx`, a row's datum word (`None`: NULL).
fn array_word(mcx: Mcx<'_>, elem: Oid, len: i16, byval: bool, elems: Option<&[Option<Datum>]>) -> Word {
    let elems = elems?;
    let values: Vec<Datum> = elems.iter().map(|v| v.unwrap_or(Datum::null())).collect();
    let nulls: Vec<bool> = elems.iter().map(|v| v.is_none()).collect();
    let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &[elems.len() as i32], &[1], elem, len.into(), byval, b'i').unwrap();
    Some(img.leak().as_ptr() as usize as u64)
}

/// The walked ScalarArrayOp (expr/src/emit/scalar.rs `array_walk`, host.rs `saop_walk`): an array that is no constant and
/// no short ARRAY[...] (a column, a parameter, an ARRAY[...] of more than 16 elements) is walked by one host call, each
/// element compared through the site's cell. PG's rules through the four evaluators: a NULL array is NULL, an empty one
/// answers before the scalar's NULL (D1812), a NULL scalar of a strict operator is NULL, a NULL element leaves the answer
/// NULL unless one decides, the first deciding element ends it, the scalar's raise comes before the walk, a CASE arm not
/// taken never walks, and text elements reach the shim as views (the region released per element).
#[test]
fn a_non_constant_array_is_walked_through_the_sites_cell() {
    let cx = Box::leak(Box::new(MemoryContext::new("emit-test array walk")));
    let m = cx.mcx();
    let ia = |e: &[Option<i32>]| array_word(m, INT4OID, 4, true, Some(&e.iter().map(|v| v.map(Datum::from_i32)).collect::<Vec<_>>()));
    let long: Vec<Option<i32>> = (1..=20).map(Some).collect();
    let rows: Vec<Vec<Word>> = vec![
        vec![i4(7), ia(&[Some(1), Some(7), None])],
        vec![i4(7), ia(&[Some(1), None])],
        vec![i4(7), ia(&[])],
        vec![n(), ia(&[])],
        vec![n(), ia(&[Some(1)])],
        vec![i4(7), n()],
        vec![i4(3), ia(&[Some(1), Some(2)])],
        vec![i4(i32::MIN), ia(&[Some(i32::MIN), Some(5)])],
        vec![i4(5), ia(&[Some(5), Some(5)])],
        vec![i4(0), ia(&[None])],
        vec![i4(20), ia(&long)],
        vec![i4(21), ia(&long)],
    ];
    let t = &[INT4OID, INT4ARRAYOID];
    fn a(m: Mcx<'_>) -> Node<'_> {
        scan_var(m, 1, INT4OID)
    }
    fn b(m: Mcx<'_>) -> Node<'_> {
        scan_var(m, 2, INT4ARRAYOID)
    }
    let p1 = ia(&[Some(7), None, Some(3)]);
    let ta = |e: &[Option<&str>]| array_word(m, TEXTOID, -1, false, Some(&e.iter().map(|v| v.map(|s| text_image(m, s))).collect::<Vec<_>>()));
    let tw = |s: &str| Some(text_image(m, s).as_u64());
    let trows: Vec<Vec<Word>> = vec![
        vec![tw("abc"), ta(&[Some("x"), Some("abc")])],
        vec![tw("abc"), ta(&[Some("x"), None])],
        vec![tw(""), ta(&[Some("")])],
        vec![n(), ta(&[Some("a")])],
        vec![tw("héllo"), ta(&[Some("hello"), Some("héllo")])],
        vec![tw("abc"), n()],
        vec![tw("abc"), ta(&[])],
    ];
    let tt = &[TEXTOID, TEXTARRAYOID];
    let cases = vec![
        tc("a = any (b)", t, &rows, BOOLOID, |m| saop(m, true, a(m), b(m))),
        tc("a = all (b)", t, &rows, BOOLOID, |m| saop(m, false, a(m), b(m))),
        tc("a <> all (b)", t, &rows, BOOLOID, |m| saop_by(m, 518, 144, 0, false, a(m), b(m))),
        tc("a < any (b)", t, &rows, BOOLOID, |m| saop_by(m, 97, 66, 0, true, a(m), b(m))),
        tc("not (a = any (b))", t, &rows, BOOLOID, |m| boolexpr(m, BoolExprType::NOT_EXPR, &[saop(m, true, a(m), b(m))])),
        tc("a / 0 = any (b): the scalar's raise before the walk", t, &rows, BOOLOID, |m| saop(m, true, opexpr(m, 154, INT4OID, 0, &[a(m), int4(m, Some(0))]), b(m))),
        tc("case when a > 5 then a = any (b) else false end", t, &rows, BOOLOID, |m| case(m, None, &[when(m, opexpr(m, 147, BOOLOID, 0, &[a(m), int4(m, Some(5))]), saop(m, true, a(m), b(m)))], Some(boolc(m, Some(false))))),
        tc("a = any (ARRAY of 20 with a null: walked)", t, &rows, BOOLOID, |m| {
            let mut e: Vec<Node<'_>> = (0..19).map(|i| int4(m, Some(i * 2))).collect();
            e.push(int4(m, None));
            saop(m, true, a(m), array_md(m, &e, false))
        }),
        Case { extern_params: vec![(INT4ARRAYOID, p1)], ..tc("a = any ($1): a parameter array", t, &rows, BOOLOID, |m| saop(m, true, a(m), param(m, 1, INT4ARRAYOID))) },
        tc("s = any (texts): views per element", tt, &trows, BOOLOID, |m| saop_by(m, 98, 67, 950, true, scan_var(m, 1, TEXTOID), scan_var(m, 2, TEXTARRAYOID))),
        tc("s <> all (texts)", tt, &trows, BOOLOID, |m| saop_by(m, 531, 157, 950, false, scan_var(m, 1, TEXTOID), scan_var(m, 2, TEXTARRAYOID))),
    ];
    check_cases("array walk", &cases);
}

/// D2348: GREATEST / LEAST over oid compare through `btoidcmp`, which orders oids unsigned; the emit's form read it as a
/// signed int compare, so an oid at or past 2^31 ordered below 2147483647.
#[test]
fn d2348_oid_greatest_least_compare_unsigned() {
    let rows: Vec<Vec<Word>> = vec![vec![Some(4_294_967_295), Some(1)], vec![Some(2_147_483_648), Some(2_147_483_647)], vec![Some(2_147_483_647), Some(2_147_483_648)], vec![Some(0), Some(0)], vec![n(), Some(5)], vec![Some(7), n()]];
    let t = &[OIDOID, OIDOID];
    let cases = vec![
        tc("btoidcmp(a, b)", t, &rows, INT4OID, |m| funcexpr(m, 356, INT4OID, 0, &[scan_var(m, 1, OIDOID), scan_var(m, 2, OIDOID)])),
        tc("greatest oid", t, &rows, OIDOID, |m| minmax_of(m, OIDOID, false, &[scan_var(m, 1, OIDOID), scan_var(m, 2, OIDOID)])),
        tc("least oid", t, &rows, OIDOID, |m| minmax_of(m, OIDOID, true, &[scan_var(m, 1, OIDOID), scan_var(m, 2, OIDOID)])),
    ];
    check_cases("D2348 oid GREATEST / LEAST", &cases);
}

/// Item 6: the call bit (`Call::volatile`) and D901's builtin IMMUTABLE test read `pg_proc` through the row type's own
/// methods, which answer as the effects home does: VOLATILE acts, STABLE and IMMUTABLE do not, an unknown function-manager
/// call acts (fail closed), an unknown direct shim is pure over its arguments.
#[test]
fn item6_func_facts_effect_tests() {
    use gravity_expr::catalog::FuncFacts;
    let f = |v: Option<u8>| FuncFacts { strict: true, retset: false, nargs: 0, volatility: v, parallel: None, node_arg: false, leakproof: None, prolang: None, body: 0 };
    for fmgr in [false, true] {
        assert!(f(Some(b'v')).call_acts(fmgr), "VOLATILE acts");
        assert!(!f(Some(b's')).call_acts(fmgr), "STABLE does not act");
        assert!(!f(Some(b'i')).call_acts(fmgr), "IMMUTABLE does not act");
    }
    assert!(f(None).call_acts(true), "an unknown function-manager call acts");
    assert!(!f(None).call_acts(false), "an unknown direct shim does not");
    assert!(f(Some(b'i')).immutable() && !f(Some(b's')).immutable() && !f(Some(b'v')).immutable() && !f(None).immutable());
    assert!(FuncFacts::builtin(1598) && FuncFacts::builtin(16383) && !FuncFacts::builtin(0) && !FuncFacts::builtin(16384));
}

/// D947's test expression `a / b` (int4div, 22012 on b = 0).
fn d947_test(m: Mcx<'_>) -> Node<'_> {
    opexpr(m, 154, INT4OID, 0, &[scan_var(m, 1, INT4OID), scan_var(m, 2, INT4OID)])
}

/// D947 (GV-D5, the Gravity verification register): PG evaluates a simple CASE's test before any arm, so a test that raises
/// is the CASE's error even when no arm reads it (a hand-built plan: the parser always builds `CaseTestExpr op v` arms).
/// The emit carried the test's raise only through a `CaseTest` read inside a condition: `CASE a / b WHEN <c = 5> THEN a
/// ELSE c END` over b = 0 returned a, valid, raise mask 0. The parser's shape runs beside it.
#[test]
fn d947_a_simple_case_raises_its_test_when_no_arm_reads_it() {
    let rows: Vec<Vec<Word>> = vec![vec![i4(1), i4(0), i4(5)], vec![i4(1), i4(2), i4(5)], vec![i4(4), i4(2), i4(3)], vec![n(), i4(0), i4(5)], vec![i4(1), i4(0), n()], vec![i4(0), i4(0), i4(3)]];
    let cases = vec![
        tc("CASE a / b WHEN <c = 5> THEN a ELSE c END", &[INT4OID; 3], &rows, INT4OID, |m| {
            let c5 = opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 3, INT4OID), int4(m, Some(5))]);
            case(m, Some(d947_test(m)), &[when(m, c5, scan_var(m, 1, INT4OID))], Some(scan_var(m, 3, INT4OID)))
        }),
        tc("CASE a / b WHEN <c = 5> THEN a END (the parser's NULL default)", &[INT4OID; 3], &rows, INT4OID, |m| {
            let c5 = opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 3, INT4OID), int4(m, Some(5))]);
            case(m, Some(d947_test(m)), &[when(m, c5, scan_var(m, 1, INT4OID))], Some(int4(m, None)))
        }),
        tc("CASE a / b WHEN 0 THEN a ELSE c END (the parser's shape)", &[INT4OID; 3], &rows, INT4OID, |m| {
            let t0 = opexpr(m, 65, BOOLOID, 0, &[case_test(m), int4(m, Some(0))]);
            case(m, Some(d947_test(m)), &[when(m, t0, scan_var(m, 1, INT4OID))], Some(scan_var(m, 3, INT4OID)))
        }),
    ];
    check_cases("D947 simple CASE test", &cases);
}

/// M8 B2c (nest.subplan, the lazy cell, L0-e): the lowerer's hook emits a lazy SubPlan where the expression reaches it;
/// the hook's code runs the subplan (a call), so the node calls a host and a guard PG never passes must branch around it.
/// Here the hook counts its runs in the frame: a SubPlan alone in an untaken CASE arm or a COALESCE argument past a
/// non-NULL one is never run (PG's ExecEvalSubPlan is never reached), a taken one runs once per row.
struct LazySubplanFrame {
    words: [u64; 2],
    calls: u64,
    consts: *const u64,
    consts_null: *const u8,
    out_val: u64,
}

struct CountingSubplan {
    ctx: Value,
}

impl gravity_expr::emit::LazySites for CountingSubplan {
    fn has(&self, _attno: i16) -> bool {
        false
    }
    fn emit(&self, _b: &mut FunctionBuilder<'_>, attno: i16) -> [Value; 3] {
        unreachable!("no lazy column {attno}")
    }
    fn has_subplan(&self, plan_id: i32) -> bool {
        plan_id == 1
    }
    fn emit_subplan(&self, b: &mut FunctionBuilder<'_>, _plan_id: i32) -> [Value; 3] {
        let calls = b.load(Type::I64, self.ctx, 16);
        let one = b.iconst(Type::I64, 1);
        let calls = b.int_bin(IntBinOp::Add, calls, one);
        b.store(self.ctx, 16, calls);
        [b.iconst(Type::I64, 42), b.bconst(true), b.iconst(Type::I64, 0)]
    }
}

fn b2c_subplan(m: Mcx<'_>) -> Node<'_> {
    Node::mk(m, types_nodes::primnodes::SubPlan {
        subLinkType: types_nodes::primnodes::SubLinkType::EXPR_SUBLINK,
        testexpr: None,
        paramIds: types_nodes::list::IntList::nil(),
        plan_id: 1,
        plan_name: Some("SubPlan 1"),
        firstColType: INT4OID,
        firstColTypmod: -1,
        firstColCollation: 0,
        useHashTable: false,
        unknownEqFalse: false,
        parallel_safe: false,
        setParam: types_nodes::list::IntList::nil(),
        parParam: types_nodes::list::IntList::nil(),
        args: NodeList::nil(),
        startup_cost: 0.0,
        per_call_cost: 0.0,
    })
    .unwrap()
}

#[test]
fn b2c_a_lazy_subplan_in_an_untaken_arm_is_never_run() {
    install_seams();
    let cx = MemoryContext::new("emit-test lazy subplan");
    let m = cx.mcx();
    // (expression, [(a: bool, k: int4 or NULL), expected value, expected runs])
    let cases: Vec<(&str, Node<'_>, Vec<([Option<u64>; 2], u64, u64)>)> = vec![
        ("CASE WHEN a THEN (SubPlan 1) ELSE 7 END", case(m, None, &[when(m, scan_var(m, 1, BOOLOID), b2c_subplan(m))], Some(int4(m, Some(7)))), vec![([Some(0), None], 7, 0), ([Some(1), None], 42, 1)]),
        ("COALESCE(k, (SubPlan 1))", coalesce(m, &[scan_var(m, 2, INT4OID), b2c_subplan(m)]), vec![([Some(0), Some(5)], 5, 0), ([Some(0), None], 42, 1)]),
    ];
    for (name, expr, rows) in cases {
        let compiled = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{name}: compile: {e}"));
        let needs = compiled.param_needs(scratch()).unwrap();
        let consts: Vec<u64> = needs.iter().map(|n| if let ParamNeed::Const(d) = n { d.value.as_u64() } else { unreachable!() }).collect();
        let consts_null = vec![0u8; consts.len()];
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("lazy_subplan", &[Type::Ptr], &[Type::I64]);
        let mut fb = pb.define(f);
        let ctx = fb.params()[0];
        let (a, k) = (fb.load(Type::I64, ctx, 0), fb.load(Type::I64, ctx, 8));
        let (cp, cnp) = (fb.load(Type::Ptr, ctx, 24), fb.load(Type::Ptr, ctx, 32));
        let zero = fb.iconst(Type::I64, 0);
        let (cv, cn): (Vec<Value>, Vec<Value>) = (0..consts.len())
            .map(|j| {
                let v = fb.load(Type::I64, cp, (j * 8) as i32);
                let nb = fb.load_zext(Type::I8, cnp, j as i32);
                (v, fb.icmp(IntCond::Ne, nb, zero))
            })
            .unzip();
        let t = fb.bconst(true);
        let k_null = fb.iconst(Type::I64, u64::MAX as i64);
        let k_valid = fb.icmp(IntCond::Ne, k, k_null);
        let (values, valids) = ([a, k], [t, k_valid]);
        let cols = ColsRef::Values { values: &values, valids: &valids };
        let hook = CountingSubplan { ctx };
        let row = RowRefs { lazy: Some(&hook), ..RowRefs::scan(cols, &cv, &cn, scratch()) };
        let l = compiled.emit(&mut fb, &row).unwrap_or_else(|e| panic!("{name}: emit: {e:?}"));
        fb.store(ctx, 40, l.value);
        fb.ret(&[zero]);
        drop(fb);
        let ir = pb.finish();
        gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{name}: verify: {e:?}"));
        for (label, code) in [("tpde", gravity_backend::mapped(&ir, f).unwrap()), ("interp", gravity_backend::interp(ir.clone(), f).unwrap())] {
            for &([a, k], want, runs) in &rows {
                let mut frame = LazySubplanFrame { words: [a.unwrap_or(0), k.map_or(u64::MAX, |k| Datum::from_i32(k as i32).as_u64())], calls: 0, consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), out_val: u64::MAX };
                // SAFETY: the frame is the ctx the function reads; its pointers are live for the call.
                assert_eq!(unsafe { code.call((&mut frame as *mut LazySubplanFrame).cast()) }, 0);
                assert_eq!((frame.calls, frame.out_val as u32 as u64), (runs, want), "{name} ({label}) a={a:?} k={k:?}: (runs, value)");
            }
        }
    }
}


/// A context for the tests' cells (leaked: they outlive the call that made them).
fn emit_cx() -> mcx::Mcx<'static> {
    Box::leak(Box::new(mcx::MemoryContext::new("emit test"))).mcx()
}
