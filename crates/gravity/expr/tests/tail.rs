//! The tail's kinds the incumbent cannot compare (expr-compiler.md §4): `CollateExpr` (the
//! incumbent refuses the node), the relabel arm of `ArrayCoerceExpr` (the incumbent expects an
//! element expression), `collate::path_of`, `domain::Check` for a bare value.

use std::sync::Once;

use datum::Datum;
use gravity_expr::nodes::collate::{path_of, Path};
use gravity_expr::nodes::domain::Check;
use gravity_expr::{Expr, PgCatalog, Region, Row};
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::Oid;
use types_nodes::list::NodeList;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::{ArrayCoerceExpr, CoercionForm, CollateExpr, OpExpr};
use types_portal::params::ParamBind;
use types_tuple::PgTypeShape;

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

const INT4OID: Oid = 23;
const BOOLOID: Oid = 16;
const TEXTOID: Oid = 25;
const INT4ARRAYOID: Oid = 1007;
const INT8ARRAYOID: Oid = 1016;
const C_COLLATION: Oid = 950;
const DOMAIN_OID: Oid = 90001;
const CONBIN_VALUE_GT_0: &str = "{OPEXPR :opno 521 :opfuncid 147 :opresulttype 16 :opretset false :opcollid 0 :inputcollid 0 :args ({COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 :location 47} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 55 :constvalue 4 [ 0 0 0 0 0 0 0 0 ]}) :location 53}";

static SEAMS: Once = Once::new();

fn shape(typlen: i16, typbyval: bool, typalign: u8, typcollation: Oid) -> PgTypeShape {
    PgTypeShape { typlen, typbyval, typalign: typalign as i8, typstorage: if typlen == -1 { b'x' as i8 } else { b'p' as i8 }, typcollation }
}

fn install_seams() {
    SEAMS.call_once(|| {
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(match typid {
                INT4OID => Some(shape(4, true, b'i', 0)),
                20 => Some(shape(8, true, b'd', 0)),
                BOOLOID => Some(shape(1, true, b'c', 0)),
                TEXTOID => Some(shape(-1, false, b'i', 100)),
                INT4ARRAYOID | INT8ARRAYOID => Some(shape(-1, false, b'i', 0)),
                _ => None,
            })
        });
        syscache_seams::pg_type_element_shape::set(|typid| {
            Ok(match typid {
                INT4ARRAYOID => Some(syscache_seams::PgTypeElementShape { typelem: INT4OID, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                INT8ARRAYOID => Some(syscache_seams::PgTypeElementShape { typelem: 20, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                _ => None,
            })
        });
        syscache_seams::pg_type_typtype::set(|typid| Ok(Some(if typid == DOMAIN_OID { b'd' as i8 } else { b'b' as i8 })));
        syscache_seams::pg_type_typnamespace::set(|_| Ok(Some(11)));
        syscache_seams::lookup_pg_proc_shape::set(|funcid| Ok((funcid == 147).then_some(syscache_seams::PgProcShape { prolang: 12, prosecdef: false, proconfig_isnull: true, pronamespace: 11, prorettype: BOOLOID, provariadic: 0, prosupport: 0, pronargs: 2, prokind: b'f' as i8, provolatile: b'i' as i8, proparallel: b's' as i8, proretset: false, proisstrict: true, proleakproof: false })));
        syscache_seams::pg_type_base_shape::set(|typid| Ok(Some(syscache_seams::PgTypeBaseShape { typtype: if typid == DOMAIN_OID { b'd' as i8 } else { b'b' as i8 }, typbasetype: if typid == DOMAIN_OID { INT4OID } else { 0 }, typtypmod: -1, typelem: 0, typsubscript: 0 })));
        syscache_seams::lookup_pg_type_typcache_shape::set(|typid| {
            let mut name = types_tuple::NameData::default();
            name.namestrcpy(if typid == DOMAIN_OID { "posint" } else { "int4" });
            Ok(matches!(typid, INT4OID | DOMAIN_OID).then_some(syscache_seams::PgTypeTypcacheShape { typname: name, typlen: 4, typbyval: true, typalign: b'i' as i8, typstorage: b'p' as i8, typtype: if typid == DOMAIN_OID { b'd' as i8 } else { b'b' as i8 }, typisdefined: true, typrelid: 0, typsubscript: 0, typelem: 0, typarray: 0, typcollation: 0 }))
        });
        syscache_seams::syscache_hash_value_typeoid::set(|typid| Ok(typid.wrapping_mul(0x9e3779b1)));
        syscache_seams::pg_type_domain_shape::set(|typid| {
            let mut n = types_tuple::NameData::default();
            n.namestrcpy(if typid == DOMAIN_OID { "posint" } else { "int4" });
            Ok(match typid {
                DOMAIN_OID => Some(syscache_seams::PgTypeDomainShape { typname: n, typnamespace: 2200, typtype: b'd' as i8, typnotnull: true, typbasetype: INT4OID }),
                INT4OID => Some(syscache_seams::PgTypeDomainShape { typname: n, typnamespace: 11, typtype: b'b' as i8, typnotnull: false, typbasetype: 0 }),
                _ => None,
            })
        });
        typcache_seams::scan_domain_check_constraints::set(|mcx, contypid| {
            let mut rows = mcx::vec_with_capacity_in(mcx, 1)?;
            if contypid == DOMAIN_OID {
                let mut cn = types_tuple::NameData::default();
                cn.namestrcpy("posint_check");
                rows.push(typcache_seams::DomainCheckRow { conname: cn, conbin: CONBIN_VALUE_GT_0 });
            }
            Ok(rows)
        });
        namespace_seams::type_is_visible::set(|typid| Ok(typid == DOMAIN_OID));
        syscache_seams::pg_namespace_nspname::set(|_| {
            let mut n = types_tuple::NameData::default();
            n.namestrcpy("public");
            Ok(Some(n))
        });
        clauses::init_seams();
        miscinit_seams::get_user_id::set(|| 10);
        aclchk_seams::object_aclcheck::set(|_, _, _, _| Ok(0));
        syscache_seams::pg_proc_proname::set(|_| Ok(None));
    });
}

fn text_const<'m>(mcx: Mcx<'m>, s: &str) -> Node<'m> {
    let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 4).unwrap();
    mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + 4)).unwrap();
    mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
    Node::mk_const(mcx, TEXTOID, -1, 100, -1, Datum::from_usize(img.leak().as_ptr() as usize), false, false).unwrap()
}

/// The expression evaluated once over `buf`'s region: a by-reference result (the relabelled array) points into
/// `buf`, so the caller owns the buffer for as long as it reads the result.
fn run(_mcx: Mcx<'_>, expr: Node<'_>, buf: &mut [u8]) -> gravity_expr::NullableDatum {
    let mut c = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).unwrap();
    let mut region = Region::over(buf);
    c.eval(&Row::EMPTY, &mut region).unwrap()
}

#[test]
fn collate_expr_is_its_argument() {
    install_seams();
    let ctx = MemoryContext::new("tail");
    let mcx = ctx.mcx();
    let coll = Node::mk(mcx, CollateExpr { arg: text_const(mcx, "abc"), collOid: C_COLLATION, location: -1 }).unwrap();
    let eq = Node::mk(mcx, OpExpr { opno: 98, opfuncid: 67, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: C_COLLATION, args: NodeList::from_slice(mcx, &[coll, text_const(mcx, "abc")]).unwrap(), location: -1 }).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    assert!(run(mcx, eq, &mut buf).value.as_bool());
    let null = Node::mk(mcx, CollateExpr { arg: Node::mk_const(mcx, TEXTOID, -1, 100, -1, Datum::null(), true, false).unwrap(), collOid: C_COLLATION, location: -1 }).unwrap();
    assert!(run(mcx, null, &mut buf).isnull);
    let c = Expr::compile(lists(), eq, &PgCatalog, ParamBind::NONE).unwrap();
    assert!(c.kinds().any(|k| k == gravity_expr::Kind::CollateExpr));
    assert_eq!(path_of(&PgCatalog, C_COLLATION).unwrap(), Path::Bytes);
}

#[test]
fn array_coerce_relabel_rewrites_the_element_type() {
    install_seams();
    let ctx = MemoryContext::new("tail");
    let mcx = ctx.mcx();
    let img = arrayfuncs::construct_md_array(mcx, &[Datum::from_i32(1), Datum::null(), Datum::from_i32(3)], Some(&[false, true, false]), 1, &[3], &[1], INT4OID, 4, true, b'i').unwrap();
    let arr = Node::mk_const(mcx, INT4ARRAYOID, -1, 0, -1, Datum::from_usize(img.leak().as_ptr() as usize), false, false).unwrap();
    let coerce = Node::mk(mcx, ArrayCoerceExpr { arg: arr, elemexpr: None, resulttype: INT8ARRAYOID, resulttypmod: -1, resultcollid: 0, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let r = run(mcx, coerce, &mut buf);
    let p = r.value.as_usize() as *const u8;
    let out = unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_4b(p)) };
    assert_eq!((arrayfuncs::arr_elemtype(out), arrayfuncs::arr_ndim(out), arrayfuncs::arr_dim(out, 0), arrayfuncs::arr_hasnull(out)), (20, 1, 3, true));
}

#[test]
fn domain_check_for_a_bare_value() {
    install_seams();
    let mut check = Check::compile(lists(), DOMAIN_OID, &PgCatalog).unwrap_or_else(|e| panic!("{e}"));
    assert!(check.notnull);
    let mut buf = vec![0u8; 1 << 12];
    let mut region = Region::over(&mut buf);
    assert!(check.check(gravity_expr::NullableDatum::value(Datum::from_i32(5)), &mut region).is_ok());
    let e = check.check(gravity_expr::NullableDatum::value(Datum::from_i32(0)), &mut region).err().unwrap();
    assert_eq!((e.sqlstate(), e.constraint_name()), (types_error::ERRCODE_CHECK_VIOLATION, Some("posint_check")));
    let e = check.check(gravity_expr::NullableDatum::null(), &mut region).err().unwrap();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_NOT_NULL_VIOLATION);
}
