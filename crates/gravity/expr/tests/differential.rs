//! The differential harness (docs/gravity/expr-compiler.md §3): the corpus of
//! `execexpr/src/tests.rs`, rebuilt over the same fixtures, through both engines, compared byte
//! for byte (by-reference through the datum's bytes), errors by sqlstate and message.

use std::collections::BTreeMap;
use std::sync::Once;

use datum::{Datum, NullableDatum};
use execexpr::{exec_eval_expr, exec_init_expr, exec_init_expr_subplans_agg, AggBind, EvalSlots};
use gravity_expr::{CompileError, Expr, Kind, PgCatalog, Region, Row};
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::Oid;
use types_error::{PgError, SqlState};
use types_nodes::list::NodeList;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::{Aggref, BoolExprType, BoolTestType, CoercionForm, MinMaxOp, NullTestType, ParamKind, SQLValueFunctionOp};
use types_portal::params::{ParamBind, ParamExecData, ParamExternData, PARAM_FLAG_CONST};
use types_slot::{SlotData, TupleSlotKind};
use types_tuple::{varatt, CompactAttribute, FormData_pg_attribute, PgTypeShape, TupleDescData, TYPALIGN_INT, TYPSTORAGE_PLAIN};

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

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
const JSONBOID: Oid = 3802;
const JSONOID: Oid = 114;
const UNKNOWNOID: Oid = 705;
const JSONPATHOID: Oid = 4072;
const INT4ARRAYOID: Oid = 1007;
const INT8ARRAYOID: Oid = 1016;
const RECORDOID: Oid = 2249;
const DOMAIN_OID: Oid = 90001;
const INLIST_DOMAIN_OID: Oid = 90002;
// the write-side forms' fixture (card E-a, `store_emitted`): a composite `comp (x int4, arr int4[])`, its array type, a
// domain over int4[] with `CHECK (VALUE[1] > 0)`
const COMP_OID: Oid = 90010;
const COMP_REL: Oid = 90011;
const COMP_ARRAY_OID: Oid = 90012;
const ARRDOM_OID: Oid = 90013;
const C_COLLATION: Oid = 950;
const NUMERICOID: Oid = 1700;
const BYTEAOID: Oid = 17;
const VARCHAROID: Oid = 1043;
const VARCHARARRAYOID: Oid = 1015;
const BPCHAROID: Oid = 1042;
const NAMEOID: Oid = 19;
const DATEOID: Oid = 1082;
const TIMEOID: Oid = 1083;
const TIMESTAMPOID: Oid = 1114;
const TIMESTAMPTZOID: Oid = 1184;
const INTERVALOID: Oid = 1186;
const UUIDOID: Oid = 2950;
const INETOID: Oid = 869;

thread_local! {
    static NEXTVAL_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ACL_DENY: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ---------------------------------------------------------------------------------------------
// The fixture: the corpus's seam installation (execexpr/src/tests.rs `install_seams`), the type
// shape table widened to every type the sweeps touch.
// ---------------------------------------------------------------------------------------------

static SEAMS: Once = Once::new();

const CONBIN_VALUE_IN_LIST: &str = "{SCALARARRAYOPEXPR :opno 96 :opfuncid 65 :hashfuncid 0 \
    :negfuncid 0 :useOr true :inputcollid 0 :args ({CONST :consttype 23 :consttypmod -1 \
    :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 \
    :constvalue 4 [ 1 0 0 0 0 0 0 0 ]} {ARRAYEXPR :array_typeid 1007 :array_collid 0 \
    :element_typeid 23 :elements ({COERCETODOMAINVALUE :typeId 23 :typeMod -1 :collation 0 \
    :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 \
    :constbyval true :constisnull false :location -1 :constvalue 4 [ 2 0 0 0 0 0 0 0 ]}) \
    :multidims false :list_start -1 :list_end -1 :location -1}) :location -1}";
const CONBIN_VALUE_GT_0: &str = "{OPEXPR :opno 521 :opfuncid 147 :opresulttype 16 \
    :opretset false :opcollid 0 :inputcollid 0 :args ({COERCETODOMAINVALUE \
    :typeId 23 :typeMod -1 :collation 0 :location 47} {CONST :consttype 23 \
    :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull \
    false :location 55 :constvalue 4 [ 0 0 0 0 0 0 0 0 ]}) :location 53}";
const CONBIN_ARR_FIRST_GT_0: &str = "{OPEXPR :opno 521 :opfuncid 147 :opresulttype 16 :opretset false :opcollid 0 \
    :inputcollid 0 :args ({SUBSCRIPTINGREF :refcontainertype 1007 :refelemtype 23 :refrestype 23 :reftypmod -1 \
    :refcollid 0 :refupperindexpr ({CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true \
    :constisnull false :location -1 :constvalue 4 [ 1 0 0 0 0 0 0 0 ]}) :reflowerindexpr <> :refexpr \
    {COERCETODOMAINVALUE :typeId 1007 :typeMod -1 :collation 0 :location -1} :refassgnexpr <>} {CONST :consttype 23 \
    :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 \
    [ 0 0 0 0 0 0 0 0 ]}) :location -1}";

fn shape(typlen: i16, typbyval: bool, typalign: u8, typcollation: Oid) -> PgTypeShape {
    PgTypeShape { typlen, typbyval, typalign: typalign as i8, typstorage: if typlen == -1 { b'x' as i8 } else { TYPSTORAGE_PLAIN }, typcollation }
}

fn install_seams() {
    SEAMS.call_once(|| {
        miscinit_seams::get_user_id::set(|| 10);
        miscinit_seams::get_session_user_id::set(|| 11);
        syscache_seams::lookup_authid_session_by_oid::set(|roleid| {
            let mut n = types_tuple::NameData::default();
            n.namestrcpy(if roleid == 11 { "session_role" } else { "current_role" });
            Ok((roleid == 11).then_some(syscache_seams::AuthIdSessionShape { roleid, rolname: n, rolsuper: false, rolcanlogin: true, rolconnlimit: -1 }))
        });
        syscache_seams::pg_proc_proname::set(|_funcid| Ok(None));
        namespace_seams::is_temp_namespace::set(|_| false);
        syscache_seams::pg_type_typnamespace::set(|_| Ok(Some(11)));
        syscache_seams::pg_type_element_shape::set(|typid| {
            Ok(match typid {
                1007 => Some(syscache_seams::PgTypeElementShape { typelem: 23, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                1016 => Some(syscache_seams::PgTypeElementShape { typelem: 20, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                1009 => Some(syscache_seams::PgTypeElementShape { typelem: 25, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                VARCHARARRAYOID => Some(syscache_seams::PgTypeElementShape { typelem: VARCHAROID, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                3802 => Some(syscache_seams::PgTypeElementShape { typelem: 0, typsubscript: 6098 }),
                COMP_ARRAY_OID => Some(syscache_seams::PgTypeElementShape { typelem: COMP_OID, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER }),
                _ => None,
            })
        });
        aclchk_seams::object_aclcheck::set(|_classid, objid, _roleid, _mode| Ok(if ACL_DENY.with(|d| d.borrow().contains(&objid)) { 2 } else { 0 }));
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(match typid {
                INT4OID => Some(shape(4, true, b'i', 0)),
                INT2OID => Some(shape(2, true, b's', 0)),
                INT8OID => Some(shape(8, true, b'd', 0)),
                OIDOID => Some(shape(4, true, b'i', 0)),
                FLOAT4OID => Some(shape(4, true, b'i', 0)),
                FLOAT8OID => Some(shape(8, true, b'd', 0)),
                BOOLOID => Some(shape(1, true, b'c', 0)),
                TEXTOID => Some(shape(-1, false, b'i', 100)),
                INT4ARRAYOID | 1016 => Some(shape(-1, false, b'i', 0)),
                1009 | VARCHARARRAYOID => Some(shape(-1, false, b'i', 100)),
                RECORDOID => Some(shape(-1, false, b'd', 0)),
                JSONBOID | JSONOID => Some(shape(-1, false, b'i', 0)),
                UNKNOWNOID => Some(shape(-2, false, b'c', 0)),
                NUMERICOID | BYTEAOID | INETOID => Some(shape(-1, false, b'i', 0)),
                VARCHAROID | BPCHAROID => Some(shape(-1, false, b'i', 100)),
                NAMEOID => Some(shape(64, false, b'c', 950)),
                DATEOID => Some(shape(4, true, b'i', 0)),
                TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID => Some(shape(8, true, b'd', 0)),
                INTERVALOID => Some(shape(16, false, b'd', 0)),
                UUIDOID => Some(shape(16, false, b'c', 0)),
                1266 => Some(shape(12, false, b'd', 0)),
                27 => Some(shape(6, false, b's', 0)),
                28 | 29 => Some(shape(4, true, b'i', 0)),
                COMP_OID | COMP_ARRAY_OID => Some(shape(-1, false, b'd', 0)),
                ARRDOM_OID => Some(shape(-1, false, b'i', 0)),
                _ => None,
            })
        });
        const INT4_BTREE_OPCLASS: u32 = 1978;
        const INT_BTREE_FAM: u32 = 1976;
        const TEXT_BTREE_FAM: u32 = 1994;
        const TEXT_BTREE_OPCLASS: u32 = 3126;
        const F_BTINT4CMP: u32 = 351;
        syscache_seams::lookup_pg_type_typcache_shape::set(|typid| {
            Ok(match typid {
                INT4OID | DOMAIN_OID | INLIST_DOMAIN_OID => {
                    let mut name = types_tuple::NameData::default();
                    name.namestrcpy(match typid {
                        INT4OID => "int4",
                        DOMAIN_OID => "posint",
                        _ => "inlist",
                    });
                    Some(syscache_seams::PgTypeTypcacheShape {
                        typname: name,
                        typlen: 4,
                        typbyval: true,
                        typalign: b'i' as i8,
                        typstorage: b'p' as i8,
                        typtype: if typid == INT4OID { b'b' as i8 } else { b'd' as i8 },
                        typisdefined: true,
                        typrelid: 0,
                        typsubscript: 0,
                        typelem: 0,
                        typarray: 0,
                        typcollation: 0,
                    })
                }
                // the store fixture: the composite (its descriptor from the fake relation `COMP_REL`), int4[] under the
                // array domain, the domain
                COMP_OID | INT4ARRAYOID | ARRDOM_OID => {
                    let mut name = types_tuple::NameData::default();
                    name.namestrcpy(match typid {
                        COMP_OID => "comp",
                        INT4ARRAYOID => "_int4",
                        _ => "arrdom",
                    });
                    Some(syscache_seams::PgTypeTypcacheShape {
                        typname: name,
                        typlen: -1,
                        typbyval: false,
                        typalign: if typid == COMP_OID { b'd' as i8 } else { b'i' as i8 },
                        typstorage: b'x' as i8,
                        typtype: match typid {
                            COMP_OID => b'c' as i8,
                            INT4ARRAYOID => b'b' as i8,
                            _ => b'd' as i8,
                        },
                        typisdefined: true,
                        typrelid: if typid == COMP_OID { COMP_REL } else { 0 },
                        typsubscript: if typid == INT4ARRAYOID { lsyscache::F_ARRAY_SUBSCRIPT_HANDLER } else { 0 },
                        typelem: if typid == INT4ARRAYOID { INT4OID } else { 0 },
                        typarray: 0,
                        typcollation: 0,
                    })
                }
                // text: the by-reference GREATEST/LEAST cases' comparison (typcache CMP_PROC through text_ops)
                TEXTOID => {
                    let mut name = types_tuple::NameData::default();
                    name.namestrcpy("text");
                    Some(syscache_seams::PgTypeTypcacheShape {
                        typname: name,
                        typlen: -1,
                        typbyval: false,
                        typalign: b'i' as i8,
                        typstorage: b'x' as i8,
                        typtype: b'b' as i8,
                        typisdefined: true,
                        typrelid: 0,
                        typsubscript: 0,
                        typelem: 0,
                        typarray: 0,
                        typcollation: 100,
                    })
                }
                _ => None,
            })
        });
        syscache_seams::syscache_hash_value_typeoid::set(|typid| Ok(typid.wrapping_mul(0x9e3779b1)));
        syscache_seams::lookup_pg_opclass_shape::set(|opclass| {
            Ok(match opclass {
                INT4_BTREE_OPCLASS => Some((INT_BTREE_FAM, INT4OID)),
                TEXT_BTREE_OPCLASS => Some((TEXT_BTREE_FAM, TEXTOID)),
                _ => None,
            }
            .map(|(opcfamily, opcintype)| syscache_seams::PgOpclassShape { opcmethod: types_core::BTREE_AM_OID, opcfamily, opcintype, opckeytype: types_core::InvalidOid }))
        });
        syscache_seams::lookup_pg_amop_by_operator::set(|opno, _purpose, opfamily| {
            Ok(match (opno, opfamily) {
                (97, INT_BTREE_FAM) => Some(syscache_seams::PgAmopShape { amopstrategy: 1, amopsortfamily: 0, amoplefttype: INT4OID, amoprighttype: INT4OID }),
                // text_lt in text_ops (the by-reference RowCompare cases)
                (664, TEXT_BTREE_FAM) => Some(syscache_seams::PgAmopShape { amopstrategy: 1, amopsortfamily: 0, amoplefttype: TEXTOID, amoprighttype: TEXTOID }),
                _ => None,
            })
        });
        syscache_seams::lookup_pg_amproc::set(|opfamily, _l, _r, procnum| {
            Ok(match (opfamily, procnum) {
                (INT_BTREE_FAM, 1) => F_BTINT4CMP,
                (TEXT_BTREE_FAM, 1) => 360,
                _ => 0,
            })
        });
        syscache_seams::lookup_pg_operator_shape::set(|opno| {
            if opno == 664 {
                let s = syscache_seams::PgOperatorShape { oprnamespace: 11, oprleft: TEXTOID, oprright: TEXTOID, oprresult: BOOLOID, oprcom: 666, oprnegate: 667, oprcode: 740, oprrest: 0, oprjoin: 0, oprcanmerge: false, oprcanhash: false };
                return Ok(Some(s));
            }
            Ok((opno == 97).then_some(syscache_seams::PgOperatorShape {
                oprnamespace: 11,
                oprleft: INT4OID,
                oprright: INT4OID,
                oprresult: BOOLOID,
                oprcom: 521,
                oprnegate: 525,
                oprcode: 66,
                oprrest: 0,
                oprjoin: 0,
                oprcanmerge: false,
                oprcanhash: false,
            }))
        });
        indexcmds_seams::get_default_opclass::set(|type_id, am_id| Ok(match (type_id, am_id == types_core::BTREE_AM_OID) {
            (INT4OID, true) => INT4_BTREE_OPCLASS,
            (TEXTOID, true) => TEXT_BTREE_OPCLASS,
            _ => 0,
        }));
        install_domain_seams();
        install_json_seams();
        detoast_seams::detoast_attr::set(|mcx, raw| {
            if raw[0] & 0x01 == 0x01 && raw[0] != 0x01 {
                let total = (raw[0] >> 1) as usize;
                let data = &raw[1..total];
                let mut v = mcx::vec_with_capacity_in(mcx, data.len() + 4)?;
                mcx::vec_append_bytes(&mut v, &datum::set_varsize_4b(data.len() + 4))?;
                mcx::vec_append_bytes(&mut v, data)?;
                Ok(v)
            } else {
                let mut v = mcx::vec_with_capacity_in(mcx, raw.len())?;
                mcx::vec_append_bytes(&mut v, raw)?;
                Ok(v)
            }
        });
        install_tz();
        // the store fixture counts the calls per thread (store_emitted: one call per row in every engine)
        sequence_seams::nextval_internal::set(|seqid, _check| {
            NEXTVAL_CALLS.with(|c| c.set(c.get() + 1));
            Ok(seqid as i64 * 3)
        });
        relation_seams::relation_open::set(|mcx, relid, _lockmode| {
            assert_eq!(relid, COMP_REL, "the fixture's only relation");
            Ok(types_rel::Relation::open(store_emitted::comp_relation(mcx), None))
        });
    });
}

fn install_json_seams() {
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    mbutils::init_seams();
    fmgr_core::init_seams();
    funcapi::init_seams();
    postgres_seams::check_for_interrupts::set(|| Ok(()));
    syscache_seams::pg_type_typtype::set(|typid| {
        Ok(match typid {
            INT4OID | BOOLOID | TEXTOID | JSONBOID | JSONPATHOID | INT2OID | INT8OID | OIDOID | FLOAT4OID | FLOAT8OID | INT4ARRAYOID | NUMERICOID | BYTEAOID | VARCHAROID | BPCHAROID | NAMEOID | DATEOID | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID | INTERVALOID | UUIDOID | INETOID => Some(b'b' as i8),
            DOMAIN_OID | INLIST_DOMAIN_OID | ARRDOM_OID => Some(b'd' as i8),
            COMP_OID => Some(b'c' as i8),
            COMP_ARRAY_OID => Some(b'b' as i8),
            _ => None,
        })
    });
    syscache_seams::pg_type_base_shape::set(|typid| {
        Ok(matches!(typid, INT4OID | BOOLOID | TEXTOID | JSONBOID | INT2OID | INT8OID | OIDOID | FLOAT4OID | FLOAT8OID | NUMERICOID | BYTEAOID | VARCHAROID | BPCHAROID | NAMEOID | DATEOID | TIMEOID | TIMESTAMPOID | TIMESTAMPTZOID | INTERVALOID | UUIDOID | INETOID).then_some(syscache_seams::PgTypeBaseShape {
            typtype: b'b' as i8,
            typbasetype: 0,
            typtypmod: -1,
            typelem: 0,
            typsubscript: 0,
        }))
    });
    syscache_seams::pg_type_io_shape::set(|typid| {
        let mk = |typinput, typoutput, typlen, typbyval| syscache_seams::PgTypeIoShape {
            oid: typid,
            typinput,
            typoutput,
            typreceive: 0,
            typsend: 0,
            typmodin: 0,
            typmodout: 0,
            typelem: 0,
            typlen,
            typbyval,
            typalign: b'i' as i8,
            typdelim: b',' as i8,
            typisdefined: true,
        };
        Ok(match typid {
            INT4OID => Some(mk(42, 43, 4, true)),
            INT8OID => Some(mk(460, 461, 8, true)),
            TEXTOID => Some(mk(46, 47, -1, false)),
            JSONBOID => Some(mk(3806, 3805, -1, false)),
            UNKNOWNOID => Some(mk(109, 110, -2, false)),
            NUMERICOID => Some(mk(1701, 1702, -1, false)),
            BYTEAOID => Some(mk(1244, 31, -1, false)),
            VARCHAROID => Some(mk(1046, 1047, -1, false)),
            BPCHAROID => Some(mk(1044, 1045, -1, false)),
            NAMEOID => Some(mk(34, 35, 64, false)),
            DATEOID => Some(mk(1084, 1085, 4, true)),
            TIMEOID => Some(mk(1143, 1144, 8, true)),
            TIMESTAMPOID => Some(mk(1312, 1313, 8, true)),
            TIMESTAMPTZOID => Some(mk(1150, 1151, 8, true)),
            INTERVALOID => Some(mk(1160, 1161, 16, false)),
            UUIDOID => Some(mk(2952, 2953, 16, false)),
            INETOID => Some(mk(910, 911, -1, false)),
            _ => None,
        })
    });
}

fn install_domain_seams() {
    clauses::init_seams();
    syscache_seams::pg_type_domain_shape::set(|typid| {
        let mk = |nm: &str, nsp, tt, nn, base| {
            let mut n = types_tuple::NameData::default();
            n.namestrcpy(nm);
            syscache_seams::PgTypeDomainShape { typname: n, typnamespace: nsp, typtype: tt, typnotnull: nn, typbasetype: base }
        };
        Ok(match typid {
            DOMAIN_OID => Some(mk("posint", 2200, b'd' as i8, true, INT4OID)),
            ARRDOM_OID => Some(mk("arrdom", 2200, b'd' as i8, false, INT4ARRAYOID)),
            INT4ARRAYOID => Some(mk("_int4", 11, b'b' as i8, false, 0)),
            INLIST_DOMAIN_OID => Some(mk("inlist", 2200, b'd' as i8, true, INT4OID)),
            INT4OID => Some(mk("int4", 11, b'b' as i8, false, 0)),
            _ => None,
        })
    });
    typcache_seams::scan_domain_check_constraints::set(|mcx, contypid| {
        let mut rows = mcx::vec_with_capacity_in(mcx, 1)?;
        if contypid == DOMAIN_OID {
            let mut cn = types_tuple::NameData::default();
            cn.namestrcpy("posint_check");
            rows.push(typcache_seams::DomainCheckRow { conname: cn, conbin: CONBIN_VALUE_GT_0 });
        } else if contypid == ARRDOM_OID {
            let mut cn = types_tuple::NameData::default();
            cn.namestrcpy("arrdom_check");
            rows.push(typcache_seams::DomainCheckRow { conname: cn, conbin: CONBIN_ARR_FIRST_GT_0 });
        } else if contypid == INLIST_DOMAIN_OID {
            let mut cn = types_tuple::NameData::default();
            cn.namestrcpy("inlist_check");
            rows.push(typcache_seams::DomainCheckRow { conname: cn, conbin: CONBIN_VALUE_IN_LIST });
        }
        Ok(rows)
    });
    syscache_seams::lookup_pg_proc_shape::set(|funcid| {
        Ok((funcid == 147).then_some(syscache_seams::PgProcShape {
            prolang: 12,
            prosecdef: false,
            proconfig_isnull: true,
            pronamespace: 11,
            prorettype: BOOLOID,
            provariadic: 0,
            prosupport: 0,
            pronargs: 2,
            prokind: b'f' as i8,
            provolatile: b'i' as i8,
            proparallel: b's' as i8,
            proretset: false,
            proisstrict: true,
            proleakproof: false,
        }))
    });
    namespace_seams::type_is_visible::set(|typid| Ok(typid == DOMAIN_OID || typid == INLIST_DOMAIN_OID || typid == ARRDOM_OID));
    syscache_seams::pg_namespace_nspname::set(|nspid| {
        let mut n = types_tuple::NameData::default();
        n.namestrcpy(if nspid == 2200 { "public" } else { "pg_catalog" });
        Ok(Some(n))
    });
}

fn install_tz() {
    // SAFETY: single-threaded init before any getenv (the corpus's precedent).
    unsafe { std::env::set_var("PGRUST_TZDIR", "/usr/share/zoneinfo") };
    pgtz::init_seams();
    adt_timestamp::init_seams();
    guc_tables::init_seams();
    elog::init_seams();
    fd::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    adt_datetime::tz::pg_timezone_initialize().unwrap();
}

// ---------------------------------------------------------------------------------------------
// Builders (the corpus's `mk_*`).
// ---------------------------------------------------------------------------------------------

fn desc_int4<'m>(mcx: Mcx<'m>, natts: i32) -> std::rc::Rc<TupleDescData<'m>> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    for i in 0..natts {
        let att = FormData_pg_attribute { attnum: (i + 1) as i16, atttypid: INT4OID, attlen: 4, attbyval: true, attalign: TYPALIGN_INT, attstorage: TYPSTORAGE_PLAIN, ..Default::default() };
        compact.push(CompactAttribute::populate_from(&att));
        attrs.push(att);
    }
    std::rc::Rc::new(TupleDescData { natts, tdtypeid: 0, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs: compact, attrs })
}

fn virtual_slot<'m>(mcx: Mcx<'m>, values: &[Option<i32>]) -> SlotData<'m> {
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::Virtual, Some(desc_int4(mcx, values.len() as i32)));
    {
        let base = slot.base_mut();
        for (i, v) in values.iter().enumerate() {
            base.tts_values[i] = v.map_or(Datum::null(), Datum::from_i32);
            base.tts_isnull[i] = v.is_none();
        }
    }
    exectuples::exec_store_virtual_tuple(&mut slot);
    slot
}

fn heap_slot<'m>(mcx: Mcx<'m>, values: &[Option<i32>]) -> SlotData<'m> {
    let desc = desc_int4(mcx, values.len() as i32);
    let mut vals = PgVec::new_in(mcx);
    let mut nulls = PgVec::new_in(mcx);
    for v in values {
        vals.push(v.map_or(Datum::null(), Datum::from_i32));
        nulls.push(v.is_none());
    }
    let tuple = heaptuple::heap_form_tuple(mcx, &desc, &vals, &nulls).unwrap();
    let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::HeapTuple, Some(desc));
    exectuples::exec_store_heap_tuple_owned(&mut slot, mcx, tuple);
    slot
}

fn scan_var(mcx: Mcx<'_>, attno: i16, typ: Oid) -> Node<'_> {
    Node::mk_var(mcx, 1, attno, typ, -1, 0, 0).unwrap()
}

fn konst(mcx: Mcx<'_>, typ: Oid, len: i32, byval: bool, v: Option<Datum>) -> Node<'_> {
    Node::mk_const(mcx, typ, -1, 0, len, v.unwrap_or(Datum::null()), v.is_none(), byval).unwrap()
}

fn int4(mcx: Mcx<'_>, v: Option<i32>) -> Node<'_> {
    konst(mcx, INT4OID, 4, true, v.map(Datum::from_i32))
}

fn boolc(mcx: Mcx<'_>, v: Option<bool>) -> Node<'_> {
    konst(mcx, BOOLOID, 1, true, v.map(Datum::from_bool))
}

fn text_const<'m>(mcx: Mcx<'m>, s: &str, short: bool) -> Node<'m> {
    let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 4).unwrap();
    if short {
        mcx::vec_append_bytes(&mut img, &[((s.len() + 1) as u8) << 1 | 1]).unwrap();
    } else {
        mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + 4)).unwrap();
    }
    mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
    let d = Datum::from_usize(img.leak().as_ptr() as usize);
    Node::mk_const(mcx, TEXTOID, -1, 100, -1, d, false, false).unwrap()
}

/// An unknown-typed literal as the parser leaves it: a cstring datum of type `unknown`.
fn unknown_const<'m>(mcx: Mcx<'m>, s: Option<&str>) -> Node<'m> {
    let d = match s {
        Some(s) => {
            let mut c: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 1).unwrap();
            mcx::vec_append_bytes(&mut c, s.as_bytes()).unwrap();
            mcx::vec_append_bytes(&mut c, &[0]).unwrap();
            Datum::from_usize(c.leak().as_ptr() as usize)
        }
        None => Datum::null(),
    };
    Node::mk_const(mcx, UNKNOWNOID, -1, 0, -2, d, s.is_none(), false).unwrap()
}

/// A typed constant through the type's input function (the function manager), its image
/// copied into the arena; a literal the input function refuses is a test bug.
fn lit<'m>(mcx: Mcx<'m>, typ: Oid, s: &str) -> Node<'m> {
    let (input, ioparam) = lsyscache::getTypeInputInfo(typ).unwrap();
    let (typlen, byval) = lsyscache::get_typlenbyval(typ).unwrap();
    let mut call = gravity_fn::FmgrCall::bind(input, 3, 0).unwrap();
    let mut c = s.as_bytes().to_vec();
    c.push(0);
    let args = [Datum::from_usize(c.as_ptr() as usize), Datum::from_u32(ioparam), Datum::from_i32(-1)];
    let v = call.call_site(&args, &[false; 3]).unwrap_or_else(|e| panic!("input of {s:?} as {typ}: {}", e.message()));
    let d = if byval {
        v.value
    } else {
        let p = v.value.as_usize() as *const u8;
        // SAFETY: the input function's result is a live image of its type.
        let n = unsafe { if typlen == -1 { varatt::varsize_any(p) } else { typlen as usize } };
        let bytes = unsafe { core::slice::from_raw_parts(p, n) };
        let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, n).unwrap();
        mcx::vec_append_bytes(&mut img, bytes).unwrap();
        Datum::from_usize(img.leak().as_ptr() as usize)
    };
    Node::mk_const(mcx, typ, -1, if matches!(typ, VARCHAROID | BPCHAROID) { 100 } else { 0 }, typlen as i32, d, false, byval).unwrap()
}

fn opexpr<'m>(mcx: Mcx<'m>, opfuncid: Oid, resulttype: Oid, inputcollid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: 0, opfuncid, opresulttype: resulttype, opretset: false, opcollid: 0, inputcollid, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn funcexpr<'m>(mcx: Mcx<'m>, funcid: Oid, resulttype: Oid, inputcollid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid, funcresulttype: resulttype, funcretset: false, funcvariadic: false, funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, funccollid: 0, inputcollid, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn nullif<'m>(mcx: Mcx<'m>, opfuncid: Oid, resulttype: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::NullIfExpr { opno: 0, opfuncid, opresulttype: resulttype, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn distinct<'m>(mcx: Mcx<'m>, opfuncid: Oid, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::DistinctExpr { opno: 0, opfuncid, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn boolexpr<'m>(mcx: Mcx<'m>, op: BoolExprType, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::BoolExpr { boolop: op, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn param(mcx: Mcx<'_>, kind: ParamKind, paramid: i32, typ: Oid) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::Param { paramkind: kind, paramid, paramtype: typ, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap()
}

fn minmax<'m>(mcx: Mcx<'m>, least: bool, vals: &[Option<i32>]) -> Node<'m> {
    let args: Vec<Node<'m>> = vals.iter().map(|v| int4(mcx, *v)).collect();
    Node::mk(mcx, types_nodes::primnodes::MinMaxExpr { minmaxtype: INT4OID, minmaxcollid: 0, inputcollid: 0, op: if least { MinMaxOp::IS_LEAST } else { MinMaxOp::IS_GREATEST }, args: NodeList::from_slice(mcx, &args).unwrap(), location: -1 }).unwrap()
}

fn svf(mcx: Mcx<'_>, op: SQLValueFunctionOp, typ: Oid, typmod: i32) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::SQLValueFunction { op, r#type: typ, typmod, location: -1 }).unwrap()
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

fn null_test_row<'a>(mcx: Mcx<'a>, arg: Node<'a>, t: NullTestType) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::NullTest { arg: Some(arg), nulltesttype: t, argisrow: true, location: -1 }).unwrap()
}

/// `ROW(l..) < ROW(r..)` over (int4, text) pairs (`cmptype` LT / LE): int4_ops / text_ops under "C".
fn row_compare_text<'m>(mcx: Mcx<'m>, cmptype: i32, l: &[Node<'m>], r: &[Node<'m>]) -> Node<'m> {
    let (mut opnos, mut fams, mut colls) = (types_nodes::list::OidList::nil(), types_nodes::list::OidList::nil(), types_nodes::list::OidList::nil());
    for (op, fam, coll) in [(97, 1976, 0), (664, 1994, C_COLLATION)] {
        opnos.lappend(mcx, op).unwrap();
        fams.lappend(mcx, fam).unwrap();
        colls.lappend(mcx, coll).unwrap();
    }
    Node::mk(mcx, types_nodes::primnodes::RowCompareExpr { cmptype, opnos, opfamilies: fams, inputcollids: colls, largs: NodeList::from_slice(mcx, l).unwrap(), rargs: NodeList::from_slice(mcx, r).unwrap() }).unwrap()
}

fn minmax_text<'m>(mcx: Mcx<'m>, least: bool, args: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::MinMaxExpr { minmaxtype: TEXTOID, minmaxcollid: C_COLLATION, inputcollid: C_COLLATION, op: if least { MinMaxOp::IS_LEAST } else { MinMaxOp::IS_GREATEST }, args: NodeList::from_slice(mcx, args).unwrap(), location: -1 }).unwrap()
}

fn bool_test<'a>(mcx: Mcx<'a>, arg: Node<'a>, t: BoolTestType) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::BooleanTest { arg: Some(arg), booltesttype: t, location: -1 }).unwrap()
}

fn relabel<'a>(mcx: Mcx<'a>, arg: Node<'a>, typ: Oid) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::RelabelType { arg, resulttype: typ, resulttypmod: -1, resultcollid: 0, relabelformat: types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap()
}

fn coerce_via_io<'a>(mcx: Mcx<'a>, arg: Node<'a>, typ: Oid) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::CoerceViaIO { arg, resulttype: typ, resultcollid: 0, coerceformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap()
}

fn domain_coercion<'a>(mcx: Mcx<'a>, arg: Node<'a>, domain: Oid) -> Node<'a> {
    Node::mk(mcx, types_nodes::primnodes::CoerceToDomain { arg, resulttype: domain, resulttypmod: -1, resultcollid: 0, coercionformat: types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap()
}

fn int4_array_const<'m>(mcx: Mcx<'m>, elems: &[Option<i32>]) -> Node<'m> {
    let values: Vec<Datum> = elems.iter().map(|v| v.map_or(Datum::null(), Datum::from_i32)).collect();
    let nulls: Vec<bool> = elems.iter().map(|v| v.is_none()).collect();
    let dims = [elems.len() as i32];
    let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &dims, &[1], INT4OID, 4, true, b'i').unwrap();
    let d = Datum::from_usize(img.leak().as_ptr() as usize);
    Node::mk_const(mcx, INT4ARRAYOID, -1, 0, -1, d, false, false).unwrap()
}

fn saop<'m>(mcx: Mcx<'m>, use_or: bool, scalar: Node<'m>, array: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: 96, opfuncid: 65, hashfuncid: 0, negfuncid: 0, useOr: use_or, inputcollid: 0, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

fn array_expr<'m>(mcx: Mcx<'m>, elems: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ArrayExpr { array_typeid: INT4ARRAYOID, array_collid: 0, element_typeid: INT4OID, elements: NodeList::from_slice(mcx, elems).unwrap(), multidims: false, list_start: -1, list_end: -1, location: -1 }).unwrap()
}

fn array_expr_md<'m>(mcx: Mcx<'m>, elems: &[Node<'m>]) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ArrayExpr { array_typeid: INT4ARRAYOID, array_collid: 0, element_typeid: INT4OID, elements: NodeList::from_slice(mcx, elems).unwrap(), multidims: true, list_start: -1, list_end: -1, location: -1 }).unwrap()
}

fn array_coerce<'m>(mcx: Mcx<'m>, arg: Node<'m>, resulttype: Oid, elemexpr: Option<Node<'m>>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ArrayCoerceExpr { arg, elemexpr, resulttype, resulttypmod: -1, resultcollid: 0, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap()
}

/// ARRAY[...] of varchar (`varchar[]`), one dimension or `multidims` over varchar[] elements.
fn varchar_array_expr<'m>(mcx: Mcx<'m>, elems: &[Node<'m>], multidims: bool) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ArrayExpr { array_typeid: VARCHARARRAYOID, array_collid: 100, element_typeid: VARCHAROID, elements: NodeList::from_slice(mcx, elems).unwrap(), multidims, list_start: -1, list_end: -1, location: -1 }).unwrap()
}

/// PG's operand of a varchar `IN (...)` / `= ANY (ARRAY[...])` under texteq: the varchar[] constructor relabelled to text[]
/// (parse_coerce.c: varchar -> text is binary coercible, so the element expression is a RelabelType over CaseTestExpr).
fn varchar_to_text<'m>(mcx: Mcx<'m>, arr: Node<'m>) -> Node<'m> {
    array_coerce(mcx, arr, 1009, Some(relabel(mcx, case_test_of(mcx, VARCHAROID), TEXTOID)))
}

/// The same constructor to varchar(n)[]: the element expression is the length check varchar(varchar, int4, bool) (669),
/// which truncates under an explicit cast and raises 22001 under an implicit one.
fn varchar_to_varchar_n<'m>(mcx: Mcx<'m>, arr: Node<'m>, n: i32, explicit: bool) -> Node<'m> {
    let check = funcexpr(mcx, 669, VARCHAROID, 0, &[case_test_of(mcx, VARCHAROID), int4(mcx, Some(n + 4)), boolc(mcx, Some(explicit))]);
    array_coerce(mcx, arr, VARCHARARRAYOID, Some(check))
}

/// `scalar = ANY (array)` under texteq (98 / 67) and the "C" collation.
fn text_any<'m>(mcx: Mcx<'m>, scalar: Node<'m>, array: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: 98, opfuncid: 67, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: C_COLLATION, args: NodeList::from_slice(mcx, &[scalar, array]).unwrap(), location: -1 }).unwrap()
}

fn case_test_of(mcx: Mcx<'_>, typ: Oid) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::CaseTestExpr { typeId: typ, typeMod: -1, collation: 0 }).unwrap()
}

fn sbsref<'m>(mcx: Mcx<'m>, container: Node<'m>, upper: &[Option<Node<'m>>], lower: &[Option<Node<'m>>], assign: Option<Node<'m>>, (ctype, etype, rtype): (Oid, Oid, Oid)) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::SubscriptingRef { refcontainertype: ctype, refelemtype: etype, refrestype: rtype, reftypmod: -1, refcollid: 0, refupperindexpr: types_nodes::OptNodeList::from_slice(mcx, upper).unwrap(), reflowerindexpr: types_nodes::OptNodeList::from_slice(mcx, lower).unwrap(), refexpr: Some(container), refassgnexpr: assign }).unwrap()
}

fn field_select<'m>(mcx: Mcx<'m>, arg: Node<'m>, fieldnum: i16, typ: Oid) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::FieldSelect { arg, fieldnum, resulttype: typ, resulttypmod: -1, resultcollid: 0 }).unwrap()
}

/// A `record` constant: an int4 row formed with a descriptor blessed in the typcache's registry.
fn record_const<'m>(mcx: Mcx<'m>, values: &[Option<i32>]) -> Node<'m> {
    let desc = record_desc(mcx, values.len());
    let vals: Vec<Datum> = values.iter().map(|v| v.map_or(Datum::null(), Datum::from_i32)).collect();
    let nulls: Vec<bool> = values.iter().map(|v| v.is_none()).collect();
    let mut tuple = heaptuple::heap_form_tuple(mcx, &desc, &vals, &nulls).unwrap();
    let len = tuple.image().len() as u32;
    let hdr = tuple.as_tuple_mut().t_data_mut();
    hdr.set_datum_length(len);
    hdr.set_type_id(RECORDOID);
    hdr.set_typmod(desc.tdtypmod);
    let d = Datum::from_usize(tuple.as_tuple().header_ptr() as usize);
    core::mem::forget(tuple);
    Node::mk_const(mcx, RECORDOID, desc.tdtypmod, 0, -1, d, false, false).unwrap()
}

fn path_const<'m>(mcx: Mcx<'m>, path: &str) -> Node<'m> {
    let img = adt_jsonpath::path::jsonpath_in(mcx, path.as_bytes(), None).unwrap().expect("hard path");
    let d = Datum::from_usize(img.as_ptr() as usize);
    core::mem::forget(img);
    Node::mk_const(mcx, JSONPATHOID, -1, 0, -1, d, false, false).unwrap()
}

fn behavior<'m>(mcx: Mcx<'m>, btype: types_nodes::primnodes::JsonBehaviorType, expr: Node<'m>) -> Node<'m> {
    Node::mk(mcx, types_nodes::primnodes::JsonBehavior { btype, expr: Some(expr), coerce: false, location: -1 }).unwrap()
}

fn null_of(mcx: Mcx<'_>, typ: Oid) -> Node<'_> {
    let (len, byval) = match typ {
        INT4OID => (4, true),
        BOOLOID => (1, true),
        _ => (-1, false),
    };
    konst(mcx, typ, len, byval, None)
}

struct JsonSpec<'m> {
    op: types_nodes::primnodes::JsonExprOp,
    doc: &'m str,
    path: &'m str,
    ret: Oid,
    use_io: bool,
    use_json: bool,
    wrapper: types_nodes::primnodes::JsonWrapper,
    omit_quotes: bool,
    on_empty: Option<(types_nodes::primnodes::JsonBehaviorType, Option<i32>)>,
    on_error: (types_nodes::primnodes::JsonBehaviorType, Option<i32>),
    passing: &'m [(&'m str, i32)],
}

fn json_expr<'m>(mcx: Mcx<'m>, s: &JsonSpec<'m>) -> Node<'m> {
    use types_nodes::primnodes::{JsonBehaviorType as JBT, JsonExpr, JsonReturning};
    let bh = |(t, v): (JBT, Option<i32>)| {
        let e = match (t, v) {
            (JBT::JSON_BEHAVIOR_TRUE, _) => boolc(mcx, Some(true)),
            (JBT::JSON_BEHAVIOR_FALSE, _) => boolc(mcx, Some(false)),
            (_, Some(v)) => int4(mcx, Some(v)),
            _ => null_of(mcx, s.ret),
        };
        behavior(mcx, t, e)
    };
    let returning: &JsonReturning<'_> = mcx::leak_in(mcx::alloc_in(mcx, JsonReturning { format: None, typid: s.ret, typmod: -1 }).unwrap());
    let mut names = PgVec::new_in(mcx);
    let mut values = PgVec::new_in(mcx);
    for &(n, v) in s.passing {
        names.push(Node::mk_string(mcx, n).unwrap());
        values.push(int4(mcx, Some(v)));
    }
    Node::mk(mcx, JsonExpr { op: s.op, column_name: None, formatted_expr: Some(jsonb_const(mcx, s.doc)), format: None, path_spec: Some(path_const(mcx, s.path)), returning: Some(returning), passing_names: NodeList::from_slice(mcx, &names).unwrap(), passing_values: NodeList::from_slice(mcx, &values).unwrap(), on_empty: s.on_empty.map(bh), on_error: Some(bh(s.on_error)), use_io_coercion: s.use_io, use_json_coercion: s.use_json, wrapper: s.wrapper, omit_quotes: s.omit_quotes, collation: 0, location: -1 }).unwrap()
}

fn record_desc<'m>(mcx: Mcx<'m>, natts: usize) -> TupleDescData<'m> {
    let mut attrs = PgVec::new_in(mcx);
    let mut compact = PgVec::new_in(mcx);
    for i in 0..natts {
        let att = FormData_pg_attribute { attnum: (i + 1) as i16, atttypid: INT4OID, attlen: 4, attbyval: true, attalign: TYPALIGN_INT, attstorage: TYPSTORAGE_PLAIN, ..Default::default() };
        compact.push(CompactAttribute::populate_from(&att));
        attrs.push(att);
    }
    let mut desc = TupleDescData { natts: natts as i32, tdtypeid: RECORDOID, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs: compact, attrs };
    typcache::assign_record_type_typmod(&mut desc).unwrap();
    desc
}

fn record_typmod(mcx: Mcx<'_>, natts: usize) -> i32 {
    record_desc(mcx, natts).tdtypmod
}

fn row_expr<'m>(mcx: Mcx<'m>, args: &[Node<'m>]) -> Node<'m> {
    let names: Vec<Node<'m>> = (0..args.len()).map(|i| Node::mk_string(mcx, ["a", "b", "c", "d"][i]).unwrap()).collect();
    Node::mk(mcx, types_nodes::primnodes::RowExpr { args: NodeList::from_slice(mcx, args).unwrap(), row_typeid: RECORDOID, row_format: CoercionForm::COERCE_EXPLICIT_CALL, colnames: NodeList::from_slice(mcx, &names).unwrap(), location: -1 }).unwrap()
}

fn field_store<'m>(mcx: Mcx<'m>, arg: Node<'m>, fields: &[(i32, Node<'m>)]) -> Node<'m> {
    let mut nums = types_nodes::list::IntList::nil();
    let mut vals = Vec::new();
    for &(f, v) in fields {
        nums.lappend(mcx, f).unwrap();
        vals.push(v);
    }
    Node::mk(mcx, types_nodes::primnodes::FieldStore { arg, newvals: NodeList::from_slice(mcx, &vals).unwrap(), fieldnums: nums, resulttype: RECORDOID }).unwrap()
}

fn row_compare<'m>(mcx: Mcx<'m>, cmptype: i32, l: &[Node<'m>], r: &[Node<'m>]) -> Node<'m> {
    let n = l.len();
    let mut opnos = types_nodes::list::OidList::nil();
    let mut fams = types_nodes::list::OidList::nil();
    let mut colls = types_nodes::list::OidList::nil();
    for _ in 0..n {
        opnos.lappend(mcx, 97).unwrap();
        fams.lappend(mcx, 1976).unwrap();
        colls.lappend(mcx, 0).unwrap();
    }
    Node::mk(mcx, types_nodes::primnodes::RowCompareExpr { cmptype, opnos, opfamilies: fams, inputcollids: colls, largs: NodeList::from_slice(mcx, l).unwrap(), rargs: NodeList::from_slice(mcx, r).unwrap() }).unwrap()
}

fn jsonb_const<'m>(mcx: Mcx<'m>, json: &str) -> Node<'m> {
    let img = adt_jsonb::io::jsonb_in(mcx, json.as_bytes(), None).unwrap().expect("hard path");
    let d = Datum::from_usize(img.as_ptr() as usize);
    core::mem::forget(img);
    Node::mk_const(mcx, JSONBOID, -1, 0, -1, d, false, false).unwrap()
}

fn json_exists<'m>(mcx: Mcx<'m>, doc: &str, path: &str) -> Node<'m> {
    use types_nodes::primnodes::{JsonBehavior, JsonBehaviorType, JsonExpr, JsonExprOp, JsonReturning, JsonWrapper};
    let img = adt_jsonpath::path::jsonpath_in(mcx, path.as_bytes(), None).unwrap().expect("hard path");
    let d = Datum::from_usize(img.as_ptr() as usize);
    core::mem::forget(img);
    let path = Node::mk_const(mcx, JSONPATHOID, -1, 0, -1, d, false, false).unwrap();
    let returning: &JsonReturning<'_> = mcx::leak_in(mcx::alloc_in(mcx, JsonReturning { format: None, typid: BOOLOID, typmod: -1 }).unwrap());
    let on_error = Node::mk(mcx, JsonBehavior { btype: JsonBehaviorType::JSON_BEHAVIOR_FALSE, expr: Some(boolc(mcx, Some(false))), coerce: false, location: -1 }).unwrap();
    Node::mk(mcx, JsonExpr {
        op: JsonExprOp::JSON_EXISTS_OP,
        column_name: None,
        formatted_expr: Some(jsonb_const(mcx, doc)),
        format: None,
        path_spec: Some(path),
        returning: Some(returning),
        passing_names: NodeList::nil(),
        passing_values: NodeList::nil(),
        on_empty: None,
        on_error: Some(on_error),
        use_io_coercion: false,
        use_json_coercion: false,
        wrapper: JsonWrapper::JSW_NONE,
        omit_quotes: false,
        collation: 0,
        location: -1,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Cases and outcomes.
// ---------------------------------------------------------------------------------------------

type Build = Box<dyn for<'m> Fn(Mcx<'m>) -> Node<'m>>;

enum Bind {
    None,
    Extern(Vec<ParamExternData>),
    Callback,
    Exec(Vec<ParamExecData>),
}

struct Case {
    name: String,
    build: Build,
    rows: Vec<Vec<Option<i32>>>,
    heap: bool,
    bind: Bind,
    /// The owning Agg's cells (int8 words, or a leaked image's address), `Aggref { aggno }` reads them.
    aggs: Vec<Option<i64>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Null,
    Value(Vec<u8>),
    Error(SqlState, String),
    Unsupported(Kind),
    Panic,
}

unsafe fn hook(arg: *mut (), _paramid: i32, _paramtype: Oid) -> Result<NullableDatum, Box<PgError>> {
    let calls = unsafe { &mut *arg.cast::<u32>() };
    *calls += 1;
    Ok(NullableDatum { value: Datum::from_i32(42), isnull: false })
}

fn bind_of<'a>(b: &'a mut Bind) -> ParamBind<'a> {
    match b {
        Bind::None => ParamBind::NONE,
        Bind::Extern(v) => ParamBind { extern_params: Some(v.as_slice()), ..ParamBind::NONE },
        Bind::Callback => ParamBind { param_callback: Some(hook), ..ParamBind::NONE },
        Bind::Exec(v) => ParamBind { extern_params: None, exec_vals: core::ptr::NonNull::new(v.as_mut_ptr()), n_exec: v.len() as u32, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None },
    }
}

/// The result's bytes: the datum word for a by-value type; the payload of a varlena (whatever
/// its header form), a cstring to its NUL, a fixed type's `typlen` bytes otherwise.
fn bytes_of(r: NullableDatum, typ: Oid) -> Outcome {
    if r.isnull {
        return Outcome::Null;
    }
    let (typlen, byval) = lsyscache::get_typlenbyval(typ).unwrap_or((8, true));
    if byval {
        return Outcome::Value(r.value.as_u64().to_ne_bytes().to_vec());
    }
    let p = r.value.as_usize() as *const u8;
    // SAFETY: a non-null by-reference result addresses a live image of its type.
    unsafe {
        let v = match typlen {
            -1 => {
                if varatt::varatt_is_1b(p) {
                    let n = varatt::varsize_1b(p);
                    core::slice::from_raw_parts(p.add(1), n - 1)
                } else {
                    let n = varatt::varsize_4b(p);
                    core::slice::from_raw_parts(p.add(4), n - 4)
                }
            }
            -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes(),
            n => core::slice::from_raw_parts(p, n as usize),
        };
        Outcome::Value(v.to_vec())
    }
}

fn err_of(e: &PgError) -> Outcome {
    Outcome::Error(e.sqlstate(), e.message().to_string())
}

fn run_incumbent<'m>(mcx: Mcx<'m>, expr: Node<'m>, bind: ParamBind<'m>, row: Option<&[Option<i32>]>, heap: bool, typ: Oid, aggs: &[Option<i64>]) -> Outcome {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut av: Vec<Datum> = aggs.iter().map(|a| a.map_or(Datum::null(), Datum::from_i64)).collect();
        let mut an: Vec<bool> = aggs.iter().map(|a| a.is_none()).collect();
        let init = if aggs.is_empty() {
            exec_init_expr(mcx, Some(expr), bind)
        } else {
            let agg = AggBind { values: core::ptr::NonNull::new(av.as_mut_ptr()).unwrap(), nulls: core::ptr::NonNull::new(an.as_mut_ptr()).unwrap(), naggs: aggs.len() as u32, grouping: None, win: None };
            exec_init_expr_subplans_agg(mcx, Some(expr), bind, None, Some(agg))
        };
        let mut state = match init {
            Ok(s) => s.unwrap(),
            Err(e) => return err_of(&e),
        };
        state.arm_result_mcx(mcx);
        let mut calls = 0u32;
        state.arm_param_callback_arg((&mut calls as *mut u32).cast());
        // a by-reference result may address the slot (a ctid: its tid cell, C's &slot->tts_tid): read it while the slot lives
        let out = match row {
            Some(vals) => {
                let mut slot = if heap { heap_slot(mcx, vals) } else { virtual_slot(mcx, vals) };
                let mut slots = EvalSlots { scan: Some(&mut slot), inner: None, outer: None };
                exec_eval_expr(&mut state, &mut slots).map(|v| bytes_of(v, typ))
            }
            None => exec_eval_expr(&mut state, &mut EvalSlots::default()).map(|v| bytes_of(v, typ)),
        };
        match out {
            Ok(o) => o,
            Err(e) => err_of(&e),
        }
    }));
    r.unwrap_or(Outcome::Panic)
}

fn run_gravity<'m>(mcx: Mcx<'m>, expr: Node<'m>, bind: ParamBind<'m>, row: Option<&[Option<i32>]>, heap: bool, typ: Oid, aggs: &[Option<i64>]) -> Outcome {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cells: Vec<NullableDatum> = aggs.iter().map(|a| NullableDatum { value: a.map_or(Datum::null(), Datum::from_i64), isnull: a.is_none() }).collect();
        let mut c = match Expr::compile(lists(), expr, &PgCatalog, bind) {
            Ok(c) => c,
            Err(CompileError::Unsupported(k)) => return Outcome::Unsupported(k),
            Err(CompileError::Pg(e)) => return err_of(&e),
        };
        let mut calls = 0u32;
        c.arm_param_callback_arg((&mut calls as *mut u32).cast());
        let mut buf = vec![0u8; 8 << 20];
        let mut region = Region::over(&mut buf);
        let out = match row {
            Some(vals) => {
                let mut slot = if heap { heap_slot(mcx, vals) } else { virtual_slot(mcx, vals) };
                exectuples::slot_getallattrs(&mut slot);
                let base = slot.base();
                let desc = base.tts_tupleDescriptor.as_ref().expect("a descriptor").clone();
                let types = vec![INT4OID; vals.len()];
                let r = Row::scan_slot(&base.tts_values[..vals.len()], &base.tts_isnull[..vals.len()], &types, &desc, &slot).with_aggs(&cells);
                c.eval(&r, &mut region).map(|v| bytes_of(v, typ))
            }
            None => c.eval(&Row::EMPTY.with_aggs(&cells), &mut region).map(|v| bytes_of(v, typ)),
        };
        match out {
            Ok(o) => o,
            Err(e) => err_of(&e),
        }
    }));
    r.unwrap_or(Outcome::Panic)
}

fn one(name: impl Into<String>, build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> Case {
    Case { name: name.into(), build: Box::new(build), rows: Vec::new(), heap: false, bind: Bind::None, aggs: Vec::new() }
}

fn rows(name: impl Into<String>, rows: &[&[Option<i32>]], build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> Case {
    Case { name: name.into(), build: Box::new(build), rows: rows.iter().map(|r| r.to_vec()).collect(), heap: false, bind: Bind::None, aggs: Vec::new() }
}

/// `Aggref { aggno }` of a builtin aggregate (count(*) 2803, sum(int4) 2108, avg(int4) 2101): the
/// owning node's cell, bound to `Case::aggs` in both engines.
fn aggref<'m>(m: Mcx<'m>, aggfnoid: Oid, aggno: i32, args: &[Oid], aggtype: Oid) -> Node<'m> {
    let mut a = Node::build::<Aggref>(m).unwrap();
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

/// A numeric image (`numeric_in`) leaked for a by-reference cell.
fn numeric_cell(s: &str) -> i64 {
    let mut call = gravity_fn::FmgrCall::bind(1701, 3, 0).unwrap();
    let mut c = s.as_bytes().to_vec();
    c.push(0);
    let v = call.call_site(&[Datum::from_usize(c.as_ptr() as usize), Datum::from_u32(0), Datum::from_i32(-1)], &[false; 3]).unwrap();
    let p = v.value.as_usize() as *const u8;
    let img = unsafe { core::slice::from_raw_parts(p, varatt::varsize_any(p)) }.to_vec();
    Box::leak(img.into_boxed_slice()).as_ptr() as i64
}

fn agg_cases(v: &mut Vec<Case>) {
    let with = |mut c: Case, aggs: &[Option<i64>]| {
        c.aggs = aggs.to_vec();
        c
    };
    fn count(m: Mcx<'_>, aggno: i32) -> Node<'_> {
        aggref(m, 2803, aggno, &[], INT8OID)
    }
    fn sum(m: Mcx<'_>, aggno: i32) -> Node<'_> {
        aggref(m, 2108, aggno, &[INT4OID], INT8OID)
    }
    v.push(with(one("aggref cell", move |m| sum(m, 0)), &[Some(42)]));
    v.push(with(one("aggref null cell", move |m| sum(m, 0)), &[None]));
    v.push(with(one("aggref second cell", move |m| count(m, 1)), &[Some(-3), Some(7)]));
    for (n, cells) in [("above", [Some(1), Some(100_001)]), ("at", [Some(1), Some(100_000)]), ("below", [Some(1), Some(99)])] {
        v.push(with(one(format!("having count(*) > 100000 {n}"), move |m| opexpr(m, 470, BOOLOID, 0, &[count(m, 1), konst(m, INT8OID, 8, true, Some(Datum::from_i64(100_000)))])), &cells));
    }
    for (k, cells) in [(0i64, [Some(1_000), Some(10)]), (3, [Some(-50), Some(4)]), (89, [None, Some(0)]), (1, [Some(i64::MAX), Some(1)]), (2, [Some(4), Some(i64::MAX / 2 + 1)])] {
        v.push(with(one(format!("sum + {k} * count"), move |m| opexpr(m, 463, INT8OID, 0, &[sum(m, 0), opexpr(m, 465, INT8OID, 0, &[konst(m, INT8OID, 8, true, Some(Datum::from_i64(k))), count(m, 1)])])), &cells));
    }
    v.push(with(one("aggref numeric avg cell", move |m| aggref(m, 2101, 2, &[INT4OID], NUMERICOID)), &[Some(1), Some(2), Some(numeric_cell("12.3456789"))]));
    v.push(with(one("coalesce over a null cell", move |m| coalesce(m, &[sum(m, 0), konst(m, INT8OID, 8, true, Some(Datum::from_i64(-1)))])), &[None]));
    for cells in [[Some(9), Some(6)], [Some(9), Some(5)], [None, Some(6)]] {
        v.push(with(one("case on the count then the sum", move |m| case(m, None, &[when(m, opexpr(m, 470, BOOLOID, 0, &[count(m, 1), konst(m, INT8OID, 8, true, Some(Datum::from_i64(5)))]), sum(m, 0))], Some(konst(m, INT8OID, 8, true, Some(Datum::from_i64(0)))))), &cells));
    }
    v.push(with(one("aggref is null", move |m| null_test(m, sum(m, 0), NullTestType::IS_NULL)), &[None]));
    v.push(with(rows("aggref beside a row var", &[&[Some(5)], &[None], &[Some(-5)]], move |m| opexpr(m, 463, INT8OID, 0, &[sum(m, 0), funcexpr(m, 481, INT8OID, 0, &[scan_var(m, 1, INT4OID)])])), &[Some(100)]));
}

fn pool_i16() -> Vec<i16> {
    vec![i16::MIN, i16::MIN + 1, -7, -1, 0, 1, 7, i16::MAX - 1, i16::MAX]
}
fn pool_i32() -> Vec<i32> {
    vec![i32::MIN, i32::MIN + 1, -32769, -32768, -7, -1, 0, 1, 7, 32767, 32768, i32::MAX - 1, i32::MAX]
}
fn pool_i64() -> Vec<i64> {
    vec![i64::MIN, i64::MIN + 1, i32::MIN as i64 - 1, i32::MIN as i64, -1, 0, 1, i32::MAX as i64, i32::MAX as i64 + 1, i64::MAX - 1, i64::MAX]
}
fn pool_u32() -> Vec<u32> {
    vec![0, 1, 2, 7, 0x7FFF_FFFE, 0x7FFF_FFFF, 0x8000_0000, 0x8000_0001, u32::MAX - 1, u32::MAX]
}
fn pool_f32() -> Vec<f32> {
    vec![f32::NAN, -f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -0.0, 1.0, -1.0, 1.5, f32::MIN, f32::MAX, f32::MIN_POSITIVE, 1e-45, -1e-45]
}
fn pool_f64() -> Vec<f64> {
    vec![f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, 1.0, -1.0, 1.5, f32::MAX as f64 * 2.0, f32::MIN as f64 * 2.0, f64::MIN, f64::MAX, f64::MIN_POSITIVE, 5e-324]
}

/// A two-argument builtin sweep over two pools of `Datum`s (typed constants).
fn sweep(out: &mut Vec<Case>, name: &str, funcid: Oid, rettype: Oid, (lt, ll, lb): (Oid, i32, bool), (rt, rl, rb): (Oid, i32, bool), lp: &[Datum], rp: &[Datum]) {
    for (i, &a) in lp.iter().enumerate() {
        for (j, &b) in rp.iter().enumerate() {
            out.push(one(format!("{name} {funcid} [{i},{j}]"), move |mcx| opexpr(mcx, funcid, rettype, 0, &[konst(mcx, lt, ll, lb, Some(a)), konst(mcx, rt, rl, rb, Some(b))])));
        }
    }
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    let all = [Some(7), Some(8), Some(-7), None];
    // --- the corpus's expression cases, test by test ---
    v.push(one("just_const_expr", |m| int4(m, Some(42))));
    v.push(rows("fused_qual_kernel_var_eq_const", &[&[Some(7)], &[Some(8)], &[None]], |m| opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(7))])));
    v.push(rows("fused_qual_kernel_commuted_const_lt_var", &[&[Some(6)], &[Some(5)], &[Some(4)]], |m| opexpr(m, 66, BOOLOID, 0, &[int4(m, Some(5)), scan_var(m, 1, INT4OID)])));
    v.push(rows("interpreter_path_matches_fused_kernel", &[&[Some(7)], &[Some(8)], &[Some(-7)], &[None]], |m| opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(7))])));
    let mut c = rows("qual_deforms_heap_tuple_through_slot_lanes", &[&[Some(1), Some(2), Some(9)], &[Some(1), Some(2), Some(8)], &[Some(1), None, None]], |m| opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 3, INT4OID), int4(m, Some(9))]));
    c.heap = true;
    v.push(c);
    v.push(rows("multi_qual_short_circuits", &[&[Some(1), Some(11)], &[Some(1), Some(10)], &[Some(2), Some(11)], &[None, Some(11)]], |m| {
        let q1 = opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]);
        let q2 = opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(10))]);
        boolexpr(m, BoolExprType::AND_EXPR, &[q1, q2])
    }));
    v.push(one("just_func_kernel_const_args", |m| opexpr(m, 177, INT4OID, 0, &[int4(m, Some(40)), int4(m, Some(2))])));
    v.push(one("nullif_equal_args_returns_null", |m| nullif(m, 65, INT4OID, &[int4(m, Some(1)), int4(m, Some(1))])));
    v.push(one("nullif_unequal_args_returns_first", |m| nullif(m, 65, INT4OID, &[int4(m, Some(1)), int4(m, Some(2))])));
    v.push(one("nullif_null_arg_returns_first_unevaluated", |m| nullif(m, 65, INT4OID, &[int4(m, None), int4(m, Some(2))])));
    v.push(one("just_func_kernel_strict_null_const", |m| opexpr(m, 177, INT4OID, 0, &[int4(m, Some(40)), int4(m, None)])));
    v.push(rows("func_strict2_with_var_arg_null_propagation", &[&[Some(40)], &[None]], |m| opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(2))])));
    v.push(rows("nested_funcexpr_two_frames", &[&[Some(39)]], |m| {
        let inner = opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]);
        opexpr(m, 177, INT4OID, 0, &[inner, int4(m, Some(2))])
    }));
    let mut c = rows("just_var_kernel_reads_deformed_lane", &[&[Some(5), Some(6)]], |m| scan_var(m, 2, INT4OID));
    c.heap = true;
    v.push(c);
    v.push(rows("still_valid_check_rejects_type_mismatch", &[&[Some(1)]], |m| scan_var(m, 1, INT8OID)));
    // params
    let mut c = one("param_extern_step_is_one_resolved_load", |m| param(m, ParamKind::PARAM_EXTERN, 1, INT4OID));
    c.bind = Bind::Extern(vec![ParamExternData { value: Datum::from_i32(42), isnull: false, pflags: PARAM_FLAG_CONST, ptype: INT4OID }]);
    v.push(c);
    let mut c = one("param_callback_step_fetches_through_the_hook_when_reached", |m| param(m, ParamKind::PARAM_EXTERN, 1, INT4OID));
    c.bind = Bind::Callback;
    v.push(c);
    let mut c = one("param_callback_step_is_not_reached_behind_a_short_circuit", |m| boolexpr(m, BoolExprType::AND_EXPR, &[boolc(m, Some(false)), param(m, ParamKind::PARAM_EXTERN, 1, BOOLOID)]));
    c.bind = Bind::Callback;
    v.push(c);
    v.push(one("param_extern_missing_value_errors_42704", |m| param(m, ParamKind::PARAM_EXTERN, 2, INT4OID)));
    let mut c = one("param_exec_step_reads_estate_slot", |m| param(m, ParamKind::PARAM_EXEC, 1, INT4OID));
    c.bind = Bind::Exec(vec![ParamExecData::EMPTY, ParamExecData { value: Datum::from_i32(7), isnull: false, exec_plan: false }]);
    v.push(c);
    let mut c = one("param_extern_type_mismatch_is_datatype_mismatch_error", |m| param(m, ParamKind::PARAM_EXTERN, 1, INT4OID));
    c.bind = Bind::Extern(vec![ParamExternData { value: Datum::from_i32(42), isnull: false, pflags: PARAM_FLAG_CONST, ptype: TEXTOID }]);
    v.push(c);
    // minmax
    v.push(one("minmax greatest 1 2 3", |m| minmax(m, false, &[Some(1), Some(2), Some(3)])));
    v.push(one("minmax least 1 2 3", |m| minmax(m, true, &[Some(1), Some(2), Some(3)])));
    v.push(one("minmax greatest nulls ignored", |m| minmax(m, false, &[None, Some(-5), None, Some(4)])));
    v.push(one("minmax least all null", |m| minmax(m, true, &[None, None])));
    // boolexpr truth tables
    let vals = [Some(true), Some(false), None];
    for a in vals {
        for b in vals {
            v.push(one(format!("bool AND {a:?} {b:?}"), move |m| boolexpr(m, BoolExprType::AND_EXPR, &[boolc(m, a), boolc(m, b)])));
            v.push(one(format!("bool OR {a:?} {b:?}"), move |m| boolexpr(m, BoolExprType::OR_EXPR, &[boolc(m, a), boolc(m, b)])));
            for c in vals {
                v.push(one(format!("bool AND3 {a:?} {b:?} {c:?}"), move |m| boolexpr(m, BoolExprType::AND_EXPR, &[boolc(m, a), boolc(m, b), boolc(m, c)])));
                v.push(one(format!("bool OR3 {a:?} {b:?} {c:?}"), move |m| boolexpr(m, BoolExprType::OR_EXPR, &[boolc(m, a), boolc(m, b), boolc(m, c)])));
            }
        }
        v.push(one(format!("bool NOT {a:?}"), move |m| boolexpr(m, BoolExprType::NOT_EXPR, &[boolc(m, a)])));
        for t in [BoolTestType::IS_TRUE, BoolTestType::IS_NOT_TRUE, BoolTestType::IS_FALSE, BoolTestType::IS_NOT_FALSE, BoolTestType::IS_UNKNOWN, BoolTestType::IS_NOT_UNKNOWN] {
            v.push(one(format!("booltest {t:?} {a:?}"), move |m| bool_test(m, boolc(m, a), t)));
        }
    }
    for a in all {
        v.push(one(format!("nulltest IS NULL {a:?}"), move |m| null_test(m, int4(m, a), NullTestType::IS_NULL)));
        v.push(one(format!("nulltest IS NOT NULL {a:?}"), move |m| null_test(m, int4(m, a), NullTestType::IS_NOT_NULL)));
        v.push(one(format!("distinct {a:?} 7"), move |m| distinct(m, 65, &[int4(m, a), int4(m, Some(7))])));
        v.push(one(format!("distinct {a:?} null"), move |m| distinct(m, 65, &[int4(m, a), int4(m, None)])));
        v.push(one(format!("coalesce {a:?} 9"), move |m| coalesce(m, &[int4(m, None), int4(m, a), int4(m, Some(9))])));
        v.push(one(format!("relabel {a:?}"), move |m| relabel(m, int4(m, a), INT4OID)));
        v.push(one(format!("coerce_via_io int4->text {a:?}"), move |m| coerce_via_io(m, int4(m, a), TEXTOID)));
        v.push(one(format!("coerce_via_io int4->int8 {a:?}"), move |m| coerce_via_io(m, int4(m, a), INT8OID)));
    }
    // a row-valued IS [NOT] NULL (the interpreter's row walk; emitted, one host call over that walk): every field decides
    for (name, fields) in [("()", vec![]), ("(null, null)", vec![None, None]), ("(1, null)", vec![Some(1), None]), ("(1, 2)", vec![Some(1), Some(2)])] {
        for (t, tn) in [(NullTestType::IS_NULL, "IS NULL"), (NullTestType::IS_NOT_NULL, "IS NOT NULL")] {
            let f = fields.clone();
            v.push(one(format!("row nulltest ROW{name} {tn}"), move |m| {
                let args: Vec<Node<'_>> = f.iter().map(|&x| int4(m, x)).collect();
                null_test_row(m, row_expr(m, &args), t)
            }));
        }
    }
    v.push(one("row nulltest NULL::record IS NULL", |m| null_test_row(m, konst(m, RECORDOID, -1, false, None), NullTestType::IS_NULL)));
    v.push(one("row nulltest NULL::record IS NOT NULL", |m| null_test_row(m, konst(m, RECORDOID, -1, false, None), NullTestType::IS_NOT_NULL)));
    // by-reference compares: RowCompare over (int4, text), GREATEST / LEAST over text (the compare site's call, CNM-75)
    for (ct, l, r) in [(1, "ABC", "DEF"), (1, "DEF", "ABC"), (1, "ABC", "ABC")] {
        v.push(one(format!("rowcompare text {ct} {l} {r}"), move |m| row_compare_text(m, ct, &[int4(m, Some(11)), text_const(m, l, false)], &[int4(m, Some(11)), text_const(m, r, false)])));
    }
    v.push(one("greatest text", |m| minmax_text(m, false, &[text_const(m, "abc", false), text_const(m, "abd", true), konst(m, TEXTOID, -1, false, None)])));
    v.push(one("least text", |m| minmax_text(m, true, &[text_const(m, "abc", false), text_const(m, "ab", true)])));
    v.push(one("coerce_via_io text->int4 '42'", |m| coerce_via_io(m, text_const(m, "42", false), INT4OID)));
    v.push(one("coerce_via_io text->int4 'x' error", |m| coerce_via_io(m, text_const(m, "x", true), INT4OID)));
    // SQLValueFunction (datetime; the corpus's ops)
    v.push(one("svf current_timestamp_n 0", |m| svf(m, SQLValueFunctionOp::SVFOP_CURRENT_TIMESTAMP_N, 1184, 0)));
    v.push(one("svf current_date", |m| svf(m, SQLValueFunctionOp::SVFOP_CURRENT_DATE, 1082, -1)));
    v.push(one("svf current_time", |m| svf(m, SQLValueFunctionOp::SVFOP_CURRENT_TIME, 1266, -1)));
    v.push(one("svf localtime_n 0", |m| svf(m, SQLValueFunctionOp::SVFOP_LOCALTIME_N, 1083, 0)));
    v.push(one("svf localtimestamp", |m| svf(m, SQLValueFunctionOp::SVFOP_LOCALTIMESTAMP, 1114, -1)));
    v.push(one("svf current_user", |m| svf(m, SQLValueFunctionOp::SVFOP_CURRENT_USER, 19, -1)));
    v.push(one("svf session_user", |m| svf(m, SQLValueFunctionOp::SVFOP_SESSION_USER, 19, -1)));
    v.push(one("svf current_role invalid oid", |m| svf(m, SQLValueFunctionOp::SVFOP_CURRENT_ROLE, 19, -1)));
    // CASE
    v.push(rows("case_expr_arg_form", &[&[Some(1)], &[Some(2)], &[Some(7)], &[None]], |m| {
        let w = |k: i32, r: i32| when(m, opexpr(m, 65, BOOLOID, 0, &[case_test(m), int4(m, Some(k))]), int4(m, Some(r)));
        case(m, Some(scan_var(m, 1, INT4OID)), &[w(1, 10), w(2, 20)], Some(int4(m, Some(30))))
    }));
    v.push(rows("case_expr_searched_form", &[&[Some(1)], &[Some(2)], &[None]], |m| {
        let w = when(m, opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]), int4(m, Some(10)));
        case(m, None, &[w], Some(int4(m, None)))
    }));
    v.push(rows("fusion_skips_jump_targets", &[&[Some(-4)], &[Some(3)], &[None]], |m| {
        let cond = opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(0))]);
        let inner = opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]);
        let then = opexpr(m, 177, INT4OID, 0, &[inner, int4(m, Some(2))]);
        case(m, None, &[when(m, cond, then)], Some(int4(m, Some(7))))
    }));
    v.push(rows("nested case with arg inside searched case", &[&[Some(1), Some(2)], &[Some(2), Some(1)], &[None, Some(1)]], |m| {
        let inner = case(m, Some(scan_var(m, 2, INT4OID)), &[when(m, opexpr(m, 65, BOOLOID, 0, &[case_test(m), int4(m, Some(1))]), int4(m, Some(100)))], Some(int4(m, Some(200))));
        let w = when(m, opexpr(m, 65, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(1))]), inner);
        case(m, None, &[w], Some(int4(m, Some(300))))
    }));
    // domains
    for a in [Some(5), Some(0), None, Some(-1)] {
        v.push(one(format!("coerce_to_domain posint {a:?}"), move |m| domain_coercion(m, int4(m, a), DOMAIN_OID)));
        v.push(one(format!("coerce_to_domain inlist {a:?}"), move |m| domain_coercion(m, int4(m, a), INLIST_DOMAIN_OID)));
    }
    v.push(one("coerce_to_domain inlist 1", |m| domain_coercion(m, int4(m, Some(1)), INLIST_DOMAIN_OID)));
    v.push(one("coerce_to_domain inlist 2", |m| domain_coercion(m, int4(m, Some(2)), INLIST_DOMAIN_OID)));
    v.push(one("coerce_to_domain inlist 7", |m| domain_coercion(m, int4(m, Some(7)), INLIST_DOMAIN_OID)));
    // scalar array op
    let saops: &[(bool, Option<i32>, &[Option<i32>])] = &[
        (true, Some(2), &[Some(1), Some(2), Some(3)]),
        (true, Some(5), &[Some(1), Some(2), Some(3)]),
        (true, Some(5), &[]),
        (false, Some(5), &[]),
        (false, Some(2), &[Some(2), Some(2)]),
        (false, Some(2), &[Some(2), Some(3)]),
        (true, None, &[Some(1)]),
        (true, Some(2), &[Some(1), None]),
        (true, Some(2), &[None, Some(2)]),
        (false, Some(2), &[Some(2), None]),
        (false, Some(2), &[None, Some(3)]),
    ];
    for &(use_or, s, elems) in saops {
        let elems = elems.to_vec();
        v.push(one(format!("saop {use_or} {s:?} {elems:?}"), move |m| saop(m, use_or, int4(m, s), int4_array_const(m, &elems))));
    }
    v.push(one("scalar_array_op_null_array_is_null", |m| saop(m, true, int4(m, Some(2)), Node::mk_const(m, INT4ARRAYOID, -1, 0, -1, Datum::null(), true, false).unwrap())));
    v.push(one("array_expr_builds_array_consumable_by_saop", |m| saop(m, true, int4(m, Some(8)), array_expr(m, &[int4(m, Some(7)), int4(m, Some(8)), int4(m, None)]))));
    v.push(one("array_expr 3 elements", |m| array_expr(m, &[int4(m, Some(7)), int4(m, Some(8)), int4(m, None)])));
    // fused chains, overflow, unary
    v.push(rows("fused_func_chain_evaluates_like_unfused", &[&[Some(5)], &[Some(-1000)], &[None]], |m| {
        let mut e = scan_var(m, 1, INT4OID);
        for k in 1..=8 {
            e = opexpr(m, 177, INT4OID, 0, &[e, int4(m, Some(k))]);
        }
        e
    }));
    v.push(rows("economy_window_skips_qual_census", &[&[Some(-1), Some(6)], &[Some(-1), Some(5)], &[Some(1), Some(6)], &[None, Some(6)], &[Some(-1), None]], |m| {
        let a = opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(0))]);
        let b = opexpr(m, 147, BOOLOID, 0, &[scan_var(m, 2, INT4OID), int4(m, Some(5))]);
        boolexpr(m, BoolExprType::AND_EXPR, &[a, b])
    }));
    v.push(rows("thin_fused_chain_overflow_error_intact", &[&[Some(1)], &[Some(-1)]], |m| opexpr(m, 177, INT4OID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(i32::MAX))])));
    v.push(rows("thin_qual_matches_general_path", &[&[Some(-1)], &[Some(0)], &[Some(1)], &[None]], |m| opexpr(m, 66, BOOLOID, 0, &[scan_var(m, 1, INT4OID), int4(m, Some(0))])));
    v.push(rows("thin_strict1_single_rewrite", &[&[Some(5)], &[None], &[Some(i32::MIN)]], |m| opexpr(m, 212, INT4OID, 0, &[scan_var(m, 1, INT4OID)])));
    // row compare
    for (l, r) in [((1, 2), (1, 3)), ((1, 2), (1, 2)), ((2, 0), (1, 9)), ((1, 2), (0, 9))] {
        for ct in [1, 2, 4, 5] {
            v.push(one(format!("rowcompare {ct} {l:?} {r:?}"), move |m| row_compare(m, ct, &[int4(m, Some(l.0)), int4(m, Some(l.1))], &[int4(m, Some(r.0)), int4(m, Some(r.1))])));
        }
    }
    v.push(one("rowcompare null column", |m| row_compare(m, 1, &[int4(m, Some(1)), int4(m, None)], &[int4(m, Some(1)), int4(m, Some(2))])));
    // refused kinds the corpus exercises
    v.push(rows("whole-row var", &[&[Some(7), Some(8)]], |m| Node::mk_var(m, 1, 0, RECORDOID, -1, 0, 0).unwrap()));
    v.push(rows("system column var", &[&[Some(7)]], |m| Node::mk_var(m, 1, -1, 27, -1, 0, 0).unwrap()));
    for (attno, typ) in [(-1i16, 27u32), (-2, 28), (-3, 29), (-5, 29), (-6, OIDOID)] {
        let mut c = rows(format!("system column var heap {attno}"), &[&[Some(7), Some(8)]], move |m| Node::mk_var(m, 1, attno, typ, -1, 0, 0).unwrap());
        c.heap = true;
        v.push(c);
        v.push(rows(format!("system column var virtual {attno}"), &[&[Some(7)]], move |m| Node::mk_var(m, 1, attno, typ, -1, 0, 0).unwrap()));
    }
    v.push(one("current_of_expr_compiles_and_errors_cleanly_at_eval", |m| Node::mk(m, types_nodes::primnodes::CurrentOfExpr { cvarno: 1, cursor_name: Some("c1"), cursor_param: 0 }).unwrap()));
    v.push(one("subscripting_ref_without_handler_is_datatype_mismatch_error", |m| {
        let mut upper = types_nodes::OptNodeList::nil();
        upper.lappend(m, Some(int4(m, Some(1)))).unwrap();
        Node::mk(m, types_nodes::primnodes::SubscriptingRef { refcontainertype: INT4OID, refelemtype: INT4OID, refrestype: INT4OID, reftypmod: -1, refcollid: 0, refupperindexpr: upper, reflowerindexpr: types_nodes::OptNodeList::nil(), refexpr: Some(int4(m, Some(7))), refassgnexpr: None }).unwrap()
    }));
    v.push(one("subplan_without_parent_plan_is_error", |m| {
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
    }));
    v.push(one("nextval int8", |m| Node::mk(m, types_nodes::primnodes::NextValueExpr { seqid: 4242, typeId: INT8OID }).unwrap()));
    v.push(one("nextval int4", |m| Node::mk(m, types_nodes::primnodes::NextValueExpr { seqid: 4242, typeId: INT4OID }).unwrap()));
    v.push(one("nextval int2", |m| Node::mk(m, types_nodes::primnodes::NextValueExpr { seqid: 4242, typeId: INT2OID }).unwrap()));
    v.push(one("json_exists_true", |m| json_exists(m, r#"{"a": 1}"#, "$.a")));
    v.push(one("json_exists_false", |m| json_exists(m, r#"{"a": 1}"#, "$.b")));
    // --- the type sweeps: every comparison family and the arithmetic over the corpus's pools ---
    let i16d: Vec<Datum> = pool_i16().iter().map(|&x| Datum::from_i16(x)).collect();
    let i32d: Vec<Datum> = pool_i32().iter().map(|&x| Datum::from_i32(x)).collect();
    let i64d: Vec<Datum> = pool_i64().iter().map(|&x| Datum::from_i64(x)).collect();
    let u32d: Vec<Datum> = pool_u32().iter().map(|&x| Datum::from_u32(x)).collect();
    let f32d: Vec<Datum> = pool_f32().iter().map(|&x| Datum::from_f32(x)).collect();
    let f64d: Vec<Datum> = pool_f64().iter().map(|&x| Datum::from_f64(x)).collect();
    // D2689: "char" orders its bytes as uint8; the words are sign-extended, as heap fetch_att reads them
    let chd: Vec<Datum> = [i8::MIN, -56, -1, 0, 1, 65, 122, i8::MAX].iter().map(|&x| Datum::from_char(x)).collect();
    let ch = (18, 1, true);
    let i2 = (INT2OID, 2, true);
    let i4 = (INT4OID, 4, true);
    let i8 = (INT8OID, 8, true);
    let oid = (OIDOID, 4, true);
    let f4 = (FLOAT4OID, 4, true);
    let f8 = (FLOAT8OID, 8, true);
    let fams: &[(&str, &[Oid], (Oid, i32, bool), (Oid, i32, bool), &Vec<Datum>, &Vec<Datum>)] = &[
        ("int4", &[65, 144, 66, 149, 147, 150], i4, i4, &i32d, &i32d),
        ("int8", &[467, 468, 469, 471, 470, 472], i8, i8, &i64d, &i64d),
        ("int2", &[63, 145, 64, 148, 146, 151], i2, i2, &i16d, &i16d),
        ("int84", &[474, 475, 476, 478, 477, 479], i8, i4, &i64d, &i32d),
        ("int48", &[852, 853, 854, 856, 855, 857], i4, i8, &i32d, &i64d),
        ("int24", &[158, 164, 160, 166, 162, 168], i2, i4, &i16d, &i32d),
        ("int42", &[159, 165, 161, 167, 163, 169], i4, i2, &i32d, &i16d),
        ("oid", &[184, 185, 716, 717, 1638, 1639], oid, oid, &u32d, &u32d),
        ("float4", &[287, 288, 289, 290, 291, 292], f4, f4, &f32d, &f32d),
        ("float8", &[293, 294, 295, 296, 297, 298], f8, f8, &f64d, &f64d),
        ("float48", &[299, 300, 301, 302, 303, 304], f4, f8, &f32d, &f64d),
        ("float84", &[305, 306, 307, 308, 309, 310], f8, f4, &f64d, &f32d),
        ("char", &[61, 70, 1246, 72, 73, 74], ch, ch, &chd, &chd),
    ];
    for &(name, oids, lt, rt, lp, rp) in fams {
        for &f in oids {
            sweep(&mut v, &format!("cmp {name}"), f, BOOLOID, lt, rt, lp, rp);
        }
    }
    // D2742: btcharcmp (GREATEST / LEAST / a row compare's three-way over "char") orders the bytes as uint8 too, and
    // returns their difference, as btint2cmp returns the int2 values' (a direct call shows the value)
    sweep(&mut v, "btcmp char", 358, INT4OID, ch, ch, &chd, &chd);
    sweep(&mut v, "btcmp int2", 350, INT4OID, i2, i2, &i16d, &i16d);
    sweep(&mut v, "btcmp int4", 351, INT4OID, i4, i4, &i32d, &i32d);
    let arith: &[(&str, &[Oid], Oid, (Oid, i32, bool), &Vec<Datum>)] = &[
        ("int4", &[177, 181, 141, 154, 156], INT4OID, i4, &i32d),
        ("int8", &[463, 464, 465, 466, 945], INT8OID, i8, &i64d),
        ("int2", &[176, 180, 152, 153, 155], INT2OID, i2, &i16d),
        ("float4", &[204, 205, 206, 207], FLOAT4OID, f4, &f32d),
        ("float8", &[218, 219, 216, 217], FLOAT8OID, f8, &f64d),
    ];
    for &(name, oids, rt, t, p) in arith {
        for &f in oids {
            sweep(&mut v, &format!("arith {name}"), f, rt, t, t, p, p);
        }
    }
    for (name, f, rt, t, p) in [("int4um", 212, INT4OID, i4, &i32d), ("int8um", 462, INT8OID, i8, &i64d), ("int2um", 213, INT2OID, i2, &i16d)] {
        for (i, &a) in p.iter().enumerate() {
            v.push(one(format!("unary {name} [{i}]"), move |m| opexpr(m, f, rt, 0, &[konst(m, t.0, t.1, t.2, Some(a))])));
        }
    }
    // --- text: by-reference through the shims and the function manager, C collation ---
    let texts = ["", "a", "ab", "hello world", "héllo", "x".repeat(300).as_str()].map(String::from);
    for (i, a) in texts.iter().enumerate() {
        for (j, b) in texts.iter().enumerate() {
            for short in [false, true] {
                for (label, f, rt) in [("texteq", 67u32, BOOLOID), ("textcat", 1258, TEXTOID), ("text_lt", 740, BOOLOID)] {
                    let (a, b) = (a.clone(), b.clone());
                    v.push(one(format!("{label} [{i},{j}] short={short}"), move |m| opexpr(m, f, rt, C_COLLATION, &[text_const(m, &a, short), text_const(m, &b, short)])));
                }
            }
        }
        for short in [false, true] {
            for (label, f, rt) in [("textlen", 1257u32, INT4OID), ("upper", 871, TEXTOID)] {
                let a = a.clone();
                v.push(one(format!("{label} [{i}] short={short}"), move |m| funcexpr(m, f, rt, C_COLLATION, &[text_const(m, &a, short)])));
            }
            let a = a.clone();
            v.push(one(format!("textlike %l% [{i}] short={short}"), move |m| opexpr(m, 850, BOOLOID, C_COLLATION, &[text_const(m, &a, short), text_const(m, "%l%", false)])));
        }
    }
    // --- by-reference types through the shims and the function manager: literals through the
    // input functions, every pair of a pool through the family's functions ---
    let byref: &[(&str, Oid, &[&str], &[(&str, Oid, Oid, Oid)])] = &[
        ("numeric", NUMERICOID, &["0", "1", "-1", "3.14159", "-0.5", "1e30", "NaN", "Infinity", "-Infinity", "99999999999999999999.999999"], &[("add", 1724, NUMERICOID, 0), ("sub", 1725, NUMERICOID, 0), ("mul", 1726, NUMERICOID, 0), ("div", 1727, NUMERICOID, 0), ("eq", 1718, BOOLOID, 0), ("lt", 1722, BOOLOID, 0)]),
        ("date", DATEOID, &["2000-01-01", "1970-01-01", "4713-01-01 BC", "5874897-12-31", "infinity", "-infinity"], &[("date_mi", 1140, INT4OID, 0), ("date_eq", 1086, BOOLOID, 0)]),
        ("timestamp", TIMESTAMPOID, &["2000-01-01 00:00:00", "1999-12-31 23:59:59.999999", "294276-12-31 23:59:59", "infinity", "-infinity"], &[("timestamp_mi", 1188, INTERVALOID, 0), ("timestamp_lt", 1154, BOOLOID, 0)]),
        ("interval", INTERVALOID, &["1 day", "-1 day", "1 year 2 months 3 days 04:05:06", "178000000 years", "00:00:00.000001"], &[("interval_pl", 1169, INTERVALOID, 0), ("interval_eq", 1162, BOOLOID, 0)]),
        ("bytea", BYTEAOID, &["\\x", "\\x00", "\\xdeadbeef", "abc"], &[("byteaeq", 1948, BOOLOID, 0), ("byteacat", 2011, BYTEAOID, 0)]),
        ("bpchar", BPCHAROID, &["", "a", "a  ", "héllo", "hello world"], &[("bpchareq", 1048, BOOLOID, C_COLLATION)]),
        ("name", NAMEOID, &["", "a", "pg_catalog", "a_name_that_is_exactly_sixty_three_bytes_long_for_the_limit_xx"], &[("nameeq", 62, BOOLOID, C_COLLATION)]),
        ("uuid", UUIDOID, &["00000000-0000-0000-0000-000000000000", "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", "ffffffff-ffff-ffff-ffff-ffffffffffff"], &[("uuid_eq", 2956, BOOLOID, 0)]),
        ("inet", INETOID, &["127.0.0.1", "10.0.0.0/8", "::1", "2001:db8::/32", "192.168.1.255/24"], &[("network_eq", 920, BOOLOID, 0), ("network_lt", 921, BOOLOID, 0)]),
    ];
    for &(tname, typ, lits, fns) in byref {
        for &(fname, f, rt, coll) in fns {
            for (i, &a) in lits.iter().enumerate() {
                for (j, &b) in lits.iter().enumerate() {
                    v.push(one(format!("{tname} {fname} [{i},{j}]"), move |m| opexpr(m, f, rt, coll, &[lit(m, typ, a), lit(m, typ, b)])));
                }
            }
        }
    }
    for (i, &a) in ["2000-01-01", "4713-01-01 BC", "infinity"].iter().enumerate() {
        for (j, &n) in [0, 1, -1, 1_000_000, i32::MAX].iter().enumerate() {
            v.push(one(format!("date_pli [{i},{j}]"), move |m| opexpr(m, 1141, DATEOID, 0, &[lit(m, DATEOID, a), int4(m, Some(n))])));
        }
        for (j, &iv) in ["1 day", "-1 year", "178000000 years"].iter().enumerate() {
            v.push(one(format!("timestamp_pl_interval [{i},{j}]"), move |m| opexpr(m, 2032, TIMESTAMPOID, 0, &[lit(m, TIMESTAMPOID, &format!("{a} 12:00:00").replace(" BC 12:00:00", " 12:00:00 BC").replace("infinity 12:00:00", "infinity")), lit(m, INTERVALOID, iv)])));
        }
    }
    for (i, &a) in ["0", "-3.5", "NaN"].iter().enumerate() {
        v.push(one(format!("numeric_uminus [{i}]"), move |m| funcexpr(m, 1771, NUMERICOID, 0, &[lit(m, NUMERICOID, a)])));
        v.push(one(format!("numeric->text io [{i}]"), move |m| coerce_via_io(m, lit(m, NUMERICOID, a), TEXTOID)));
    }
    for (i, &a) in ["", "abc", "héllo"].iter().enumerate() {
        v.push(one(format!("bpcharlen [{i}]"), move |m| funcexpr(m, 1318, INT4OID, C_COLLATION, &[lit(m, BPCHAROID, a)])));
        v.push(one(format!("byteaoctetlen [{i}]"), move |m| funcexpr(m, 720, INT4OID, 0, &[lit(m, BYTEAOID, a)])));
        v.push(one(format!("varchar texteq [{i}]"), move |m| opexpr(m, 67, BOOLOID, C_COLLATION, &[lit(m, VARCHAROID, a), text_const(m, a, false)])));
    }
    v.push(one("acl denied int4pl", |m| { ACL_DENY.with(|d| d.borrow_mut().push(177)); opexpr(m, 177, INT4OID, 0, &[int4(m, Some(1)), int4(m, Some(2))]) }));
    v.push(one("acl denied cleared", |m| { ACL_DENY.with(|d| d.borrow_mut().clear()); opexpr(m, 177, INT4OID, 0, &[int4(m, Some(1)), int4(m, Some(2))]) }));
    v.push(one("texteq null", |m| opexpr(m, 67, BOOLOID, C_COLLATION, &[text_const(m, "a", false), konst(m, TEXTOID, -1, false, None)])));
    // --- the tail lane's kinds: arrays, subscripting, coercion, collation, the tuple face ---
    const A: (Oid, Oid, Oid) = (INT4ARRAYOID, INT4OID, INT4OID);
    const S: (Oid, Oid, Oid) = (INT4ARRAYOID, INT4OID, INT4ARRAYOID);
    const AA: (Oid, Oid, Oid) = (INT4ARRAYOID, INT4OID, INT4ARRAYOID);
    const J: (Oid, Oid, Oid) = (JSONBOID, JSONBOID, JSONBOID);
    v.push(one("array multidims 2x2", |m| array_expr_md(m, &[array_expr(m, &[int4(m, Some(1)), int4(m, Some(2))]), array_expr(m, &[int4(m, Some(3)), int4(m, None)])])));
    v.push(one("array multidims mismatch", |m| array_expr_md(m, &[array_expr(m, &[int4(m, Some(1)), int4(m, Some(2))]), array_expr(m, &[int4(m, Some(3))])])));
    v.push(one("array multidims empty", |m| array_expr_md(m, &[array_expr(m, &[]), array_expr(m, &[])])));
    v.push(one("array multidims null element", |m| array_expr_md(m, &[konst(m, INT4ARRAYOID, -1, false, None), array_expr(m, &[int4(m, Some(3))])])));
    v.push(one("array of 70000", |m| { let e: Vec<Node<'_>> = (0..70_000).map(|i| int4(m, Some(i))).collect(); array_expr(m, &e) }));
    v.push(one("array coerce int4->int8 elemexpr", |m| array_coerce(m, int4_array_const(m, &[Some(1), None, Some(-3)]), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))));
    // a by-reference element per call of one site (int4out -> textin): each element outlives the next call
    v.push(one("array coerce int4->text via io", |m| array_coerce(m, int4_array_const(m, &[Some(1), Some(22), None, Some(-333)]), 1009, Some(coerce_via_io(m, case_test_of(m, INT4OID), TEXTOID)))));
    v.push(one("array coerce empty", |m| array_coerce(m, int4_array_const(m, &[]), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))));
    v.push(one("array coerce null", |m| array_coerce(m, konst(m, INT4ARRAYOID, -1, false, None), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))));
    // a relabel ArrayCoerce over ARRAY[...] is the constructor of text elements (nodes/array.rs `compile_array_coerce`):
    // PG's image, its element type text; a NULL element, an empty constructor; the varchar(n) length check keeps the coercion
    v.push(one("array coerce varchar->text over ARRAY[...]", |m| varchar_to_text(m, varchar_array_expr(m, &[lit(m, VARCHAROID, "ab"), konst(m, VARCHAROID, -1, false, None), lit(m, VARCHAROID, "")], false))));
    v.push(one("array coerce varchar->text over ARRAY[NULL]", |m| varchar_to_text(m, varchar_array_expr(m, &[konst(m, VARCHAROID, -1, false, None)], false))));
    v.push(one("array coerce varchar->text over ARRAY[]", |m| varchar_to_text(m, varchar_array_expr(m, &[], false))));
    v.push(one("array coerce varchar->text over multidims", |m| varchar_to_text(m, varchar_array_expr(m, &[varchar_array_expr(m, &[lit(m, VARCHAROID, "ab")], false), varchar_array_expr(m, &[konst(m, VARCHAROID, -1, false, None)], false)], true))));
    for explicit in [true, false] {
        v.push(one(format!("array coerce varchar->varchar(2) over ARRAY[...] explicit={explicit}"), move |m| varchar_to_varchar_n(m, varchar_array_expr(m, &[lit(m, VARCHAROID, "ab"), lit(m, VARCHAROID, "abc"), konst(m, VARCHAROID, -1, false, None)], false), 2, explicit)));
    }
    for (i, s) in ["ab", "", "abc", "AB"].into_iter().enumerate() {
        v.push(one(format!("text = ANY (varchar->text over ARRAY[...]) [{i}]"), move |m| text_any(m, text_const(m, s, false), varchar_to_text(m, varchar_array_expr(m, &[lit(m, VARCHAROID, "ab"), konst(m, VARCHAROID, -1, false, None), lit(m, VARCHAROID, "")], false)))));
    }
    for (i, idx) in [Some(1), Some(2), Some(3), Some(4), Some(0), Some(-1), None].into_iter().enumerate() {
        v.push(one(format!("array fetch [{i}]"), move |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, idx))], &[], None, A)));
        v.push(one(format!("array assign [{i}]"), move |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, idx))], &[], Some(int4(m, Some(99))), AA)));
    }
    v.push(one("array fetch null container", |m| sbsref(m, konst(m, INT4ARRAYOID, -1, false, None), &[Some(int4(m, Some(1)))], &[], None, A)));
    v.push(one("array assign null container", |m| sbsref(m, konst(m, INT4ARRAYOID, -1, false, None), &[Some(int4(m, Some(2)))], &[], Some(int4(m, Some(5))), AA)));
    v.push(one("array assign null value", |m| sbsref(m, int4_array_const(m, &[Some(10), Some(20)]), &[Some(int4(m, Some(1)))], &[], Some(int4(m, None)), AA)));
    v.push(one("array assign old element", |m| sbsref(m, int4_array_const(m, &[Some(10), Some(20)]), &[Some(int4(m, Some(2)))], &[], Some(opexpr(m, 177, INT4OID, 0, &[case_test_of(m, INT4OID), int4(m, Some(1))])), AA)));
    v.push(one("array slice [1:2]", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, Some(2)))], &[Some(int4(m, Some(1)))], None, S)));
    v.push(one("array slice [:2]", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, Some(2)))], &[None], None, S)));
    v.push(one("array slice [2:]", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[None], &[Some(int4(m, Some(2)))], None, S)));
    v.push(one("array slice out of range", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, Some(9)))], &[Some(int4(m, Some(5)))], None, S)));
    v.push(one("array slice null bound", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, None))], &[Some(int4(m, Some(1)))], None, S)));
    v.push(one("array slice assign", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, Some(2)))], &[Some(int4(m, Some(1)))], Some(int4_array_const(m, &[Some(7), Some(8)])), S)));
    v.push(one("array slice assign null source", |m| sbsref(m, int4_array_const(m, &[Some(10), None, Some(30)]), &[Some(int4(m, Some(2)))], &[Some(int4(m, Some(1)))], Some(konst(m, INT4ARRAYOID, -1, false, None)), S)));
    v.push(one("array slice assign null container", |m| sbsref(m, konst(m, INT4ARRAYOID, -1, false, None), &[Some(int4(m, Some(2)))], &[Some(int4(m, Some(1)))], Some(int4_array_const(m, &[Some(7), Some(8)])), S)));
    v.push(one("array fetch of array expr", |m| sbsref(m, array_expr(m, &[int4(m, Some(5)), int4(m, Some(6))]), &[Some(int4(m, Some(2)))], &[], None, A)));
    v.push(one("subscript without handler", |m| sbsref(m, int4(m, Some(7)), &[Some(int4(m, Some(1)))], &[], None, (INT4OID, INT4OID, INT4OID))));
    v.push(one("jsonb fetch key", |m| sbsref(m, jsonb_const(m, r#"{"a": {"b": 1}, "c": [1, 2]}"#), &[Some(text_const(m, "a", false))], &[], None, J)));
    v.push(one("jsonb fetch path", |m| sbsref(m, jsonb_const(m, r#"{"a": {"b": 1}, "c": [1, 2]}"#), &[Some(text_const(m, "c", false)), Some(int4(m, Some(1)))], &[], None, J)));
    v.push(one("jsonb fetch missing", |m| sbsref(m, jsonb_const(m, r#"{"a": 1}"#), &[Some(text_const(m, "z", false))], &[], None, J)));
    v.push(one("jsonb fetch null subscript", |m| sbsref(m, jsonb_const(m, r#"{"a": 1}"#), &[Some(konst(m, TEXTOID, -1, false, None))], &[], None, J)));
    v.push(one("jsonb fetch int index", |m| sbsref(m, jsonb_const(m, r#"[10, 20, 30]"#), &[Some(int4(m, Some(1)))], &[], None, J)));
    v.push(one("jsonb assign key", |m| sbsref(m, jsonb_const(m, r#"{"a": 1}"#), &[Some(text_const(m, "b", false))], &[], Some(jsonb_const(m, "2")), J)));
    v.push(one("jsonb assign null container array", |m| sbsref(m, konst(m, JSONBOID, -1, false, None), &[Some(int4(m, Some(0)))], &[], Some(jsonb_const(m, "2")), J)));
    v.push(one("jsonb assign null subscript", |m| sbsref(m, jsonb_const(m, r#"{"a": 1}"#), &[Some(konst(m, TEXTOID, -1, false, None))], &[], Some(jsonb_const(m, "2")), J)));
    v.push(one("jsonb nested assign fetches old", |m| {
        let inner = sbsref(m, case_test_of(m, JSONBOID), &[Some(text_const(m, "b", false))], &[], Some(jsonb_const(m, "2")), J);
        sbsref(m, jsonb_const(m, r#"{"a": {"b": 1}}"#), &[Some(text_const(m, "a", false))], &[], Some(inner), J)
    }));
    v.push(one("jsonb nested assign old missing", |m| {
        let inner = sbsref(m, case_test_of(m, JSONBOID), &[Some(text_const(m, "b", false))], &[], Some(jsonb_const(m, "7")), J);
        sbsref(m, jsonb_const(m, r#"{"x": 1}"#), &[Some(text_const(m, "a", false))], &[], Some(inner), J)
    }));
    for (i, (f, t)) in [(1i16, INT4OID), (2, INT4OID), (2, INT8OID), (3, INT4OID), (-1, INT4OID), (0, INT4OID)].into_iter().enumerate() {
        v.push(one(format!("field select [{i}]"), move |m| field_select(m, record_const(m, &[Some(7), None]), f, t)));
    }
    v.push(one("field select null record", |m| field_select(m, konst(m, RECORDOID, -1, false, None), 1, INT4OID)));
    v.push(rows("whole-row var field 2", &[&[Some(7), Some(8)], &[None, Some(9)]], |m| field_select(m, Node::mk_var(m, 1, 0, RECORDOID, -1, 0, 0).unwrap(), 2, INT4OID)));
    let mut c = rows("whole-row var heap", &[&[Some(1), Some(2), Some(3)]], |m| Node::mk_var(m, 1, 0, RECORDOID, -1, 0, 0).unwrap());
    c.heap = true;
    v.push(c);
    v.push(one("row expr record", |m| row_expr(m, &[int4(m, Some(1)), int4(m, None), boolc(m, Some(true))])));
    v.push(one("row expr field", |m| field_select(m, row_expr(m, &[int4(m, Some(5)), int4(m, Some(6))]), 2, INT4OID)));
    v.push(one("field store", |m| field_store(m, record_const(m, &[Some(7), Some(8)]), &[(2, int4(m, Some(9)))])));
    v.push(one("field store null record", |m| field_store(m, konst(m, RECORDOID, record_typmod(m, 2), false, None), &[(1, int4(m, Some(4)))])));
    v.push(one("field store old value", |m| field_store(m, record_const(m, &[Some(7), Some(8)]), &[(1, opexpr(m, 177, INT4OID, 0, &[case_test_of(m, INT4OID), int4(m, Some(100))]))])));
    v.push(one("field store out of range", |m| field_store(m, record_const(m, &[Some(7), Some(8)]), &[(3, int4(m, Some(9)))])));
    // --- the SQL/JSON family: the corpus's json module ---
    {
        use types_nodes::primnodes::{JsonBehaviorType as JBT, JsonExprOp as JOP, JsonWrapper as JW};
        const NULLB: (JBT, Option<i32>) = (JBT::JSON_BEHAVIOR_NULL, None);
        fn exists(doc: &'static str, path: &'static str, on_error: (JBT, Option<i32>)) -> JsonSpec<'static> {
            JsonSpec { op: JOP::JSON_EXISTS_OP, doc, path, ret: BOOLOID, use_io: false, use_json: false, wrapper: JW::JSW_UNSPEC, omit_quotes: false, on_empty: None, on_error, passing: &[] }
        }
        fn query(doc: &'static str, path: &'static str, wrapper: JW) -> JsonSpec<'static> {
            JsonSpec { op: JOP::JSON_QUERY_OP, doc, path, ret: JSONBOID, use_io: false, use_json: false, wrapper, omit_quotes: false, on_empty: Some(NULLB), on_error: NULLB, passing: &[] }
        }
        fn value(doc: &'static str, path: &'static str, ret: Oid) -> JsonSpec<'static> {
            JsonSpec { op: JOP::JSON_VALUE_OP, doc, path, ret, use_io: ret != TEXTOID, use_json: false, wrapper: JW::JSW_UNSPEC, omit_quotes: true, on_empty: Some(NULLB), on_error: NULLB, passing: &[] }
        }
        let mut specs: Vec<(&str, JsonSpec<'static>)> = vec![
            ("json_exists true", exists(r#"{"a": 1}"#, "$.a", (JBT::JSON_BEHAVIOR_FALSE, None))),
            ("json_exists false", exists(r#"{"a": 1}"#, "$.nope", (JBT::JSON_BEHAVIOR_FALSE, None))),
            ("json_exists error suppressed", exists(r#"{"a": 1}"#, "strict $.a.b", (JBT::JSON_BEHAVIOR_FALSE, None))),
            ("json_exists error throws", exists(r#"{"a": 1}"#, "strict $.a.b", (JBT::JSON_BEHAVIOR_ERROR, None))),
            ("json_exists passing 5", JsonSpec { passing: &[("x", 5)], ..exists("3", "$ ? (@ < $x)", (JBT::JSON_BEHAVIOR_FALSE, None)) }),
            ("json_exists passing 1", JsonSpec { passing: &[("x", 1)], ..exists("3", "$ ? (@ < $x)", (JBT::JSON_BEHAVIOR_FALSE, None)) }),
            ("json_exists int coercion 1", JsonSpec { ret: INT4OID, use_json: true, ..exists(r#"{"a": 1}"#, "$.a", (JBT::JSON_BEHAVIOR_FALSE, Some(0))) }),
            ("json_exists int coercion 0", JsonSpec { ret: INT4OID, use_json: true, ..exists(r#"{"a": 1}"#, "$.nope", (JBT::JSON_BEHAVIOR_FALSE, Some(0))) }),
            ("json_query unconditional", query(r#"{"a": [1, 2, 3]}"#, "$.a[*]", JW::JSW_UNCONDITIONAL)),
            ("json_query conditional many", query(r#"{"a": [1, 2, 3]}"#, "$.a[*]", JW::JSW_CONDITIONAL)),
            ("json_query conditional one", query(r#"{"a": [1, 2, 3]}"#, "$.a[0]", JW::JSW_CONDITIONAL)),
            ("json_query none", query(r#"{"a": [1, 2, 3]}"#, "$.a", JW::JSW_NONE)),
            ("json_query none many", query(r#"{"a": [1, 2, 3]}"#, "$.a[*]", JW::JSW_NONE)),
            ("json_query on empty null", query(r#"{"a": 1}"#, "$.nope", JW::JSW_UNSPEC)),
            ("json_query omit quotes invalid", JsonSpec { omit_quotes: true, use_json: true, ..query(r#"{"a": "hi"}"#, "$.a", JW::JSW_UNSPEC) }),
            ("json_query omit quotes number", JsonSpec { omit_quotes: true, use_json: true, ..query(r#"{"a": "1"}"#, "$.a", JW::JSW_UNSPEC) }),
            ("json_query coercion int4", JsonSpec { ret: INT4OID, use_json: true, ..query(r#"{"a": 7}"#, "$.a", JW::JSW_UNSPEC) }),
            ("json_query coercion identity", JsonSpec { use_json: true, ..query(r#"{"a": [1, 2]}"#, "$.a", JW::JSW_UNSPEC) }),
            ("json_value text string", value(r#"{"a": "hi"}"#, "$.a", TEXTOID)),
            ("json_value text numeric", value(r#"{"a": 1.50}"#, "$.a", TEXTOID)),
            ("json_value text bool", value(r#"{"a": true}"#, "$.a", TEXTOID)),
            ("json_value int4 io", value(r#"{"a": 42}"#, "$.a", INT4OID)),
            ("json_value jsonb io", value(r#"{"a": "hi"}"#, "$.a", JSONBOID)),
            ("json_value io error suppressed", value(r#"{"a": "abc"}"#, "$.a", INT4OID)),
            ("json_value io error throws", JsonSpec { on_error: (JBT::JSON_BEHAVIOR_ERROR, None), ..value(r#"{"a": "abc"}"#, "$.a", INT4OID) }),
            ("json_value on error default", JsonSpec { on_error: (JBT::JSON_BEHAVIOR_DEFAULT, Some(7)), ..value(r#"{"a": "abc"}"#, "$.a", INT4OID) }),
            ("json_value on empty null", value(r#"{"a": 1}"#, "$.nope", INT4OID)),
            ("json_value on empty default", JsonSpec { on_empty: Some((JBT::JSON_BEHAVIOR_DEFAULT, Some(5))), ..value(r#"{"a": 1}"#, "$.nope", INT4OID) }),
            ("json_value on empty error", JsonSpec { on_empty: Some((JBT::JSON_BEHAVIOR_ERROR, None)), ..value(r#"{"a": 1}"#, "$.nope", INT4OID) }),
            ("json_value strict structural suppressed", value(r#"{"a": 1}"#, "strict $.a.b.c", TEXTOID)),
            ("json_value null item", value(r#"{"a": null}"#, "$.a", TEXTOID)),
            ("json_exists error unknown", exists(r#"{"a": 1}"#, "strict $.a.b", (JBT::JSON_BEHAVIOR_UNKNOWN, None))),
        ];
        for (name, spec) in specs.drain(..) {
            let spec: &'static JsonSpec<'static> = Box::leak(Box::new(spec));
            v.push(one(name, move |m| json_expr(m, spec)));
        }
    }
    v.push(one("textcat null", |m| opexpr(m, 1258, TEXTOID, C_COLLATION, &[konst(m, TEXTOID, -1, false, None), text_const(m, "a", false)])));
    // --- the node readers over an unknown-typed literal (get_fn_expr_arg_stable: extract_variadic_args converts its cstring) ---
    v.push(one("format('%s', 'x')", |m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[text_const(m, "%s", false), unknown_const(m, Some("x"))])));
    v.push(one("format('%s|%s', 'x', 7)", |m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[text_const(m, "%s|%s", false), unknown_const(m, Some("x")), int4(m, Some(7))])));
    v.push(one("format('%s', NULL)", |m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[text_const(m, "%s", false), unknown_const(m, None)])));
    v.push(one("format(NULL, 'x')", |m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[konst(m, TEXTOID, -1, false, None), unknown_const(m, Some("x"))])));
    v.push(one("concat(1, 'a')", |m| funcexpr(m, 3058, TEXTOID, C_COLLATION, &[int4(m, Some(1)), unknown_const(m, Some("a"))])));
    v.push(one("concat(1, NULL)", |m| funcexpr(m, 3058, TEXTOID, C_COLLATION, &[int4(m, Some(1)), unknown_const(m, None)])));
    v.push(rows("concat(x, 'a') per row", &[&[Some(3)], &[None]], |m| funcexpr(m, 3058, TEXTOID, C_COLLATION, &[scan_var(m, 1, INT4OID), unknown_const(m, Some("a"))])));
    v.push(one("json_build_object('a', 1)", |m| funcexpr(m, 3200, JSONOID, 0, &[unknown_const(m, Some("a")), int4(m, Some(1))])));
    v.push(one("json_build_object('a', NULL)", |m| funcexpr(m, 3200, JSONOID, 0, &[unknown_const(m, Some("a")), int4(m, None)])));
    agg_cases(&mut v);
    v
}

/// The carrier rows' outcomes by name: PostgreSQL's answer, so a same-shaped failure on both engines is never green.
fn pinned(name: &str, row: Option<&[Option<i32>]>) -> Option<Outcome> {
    let text = |s: &str| Some(Outcome::Value(s.as_bytes().to_vec()));
    let boolean = |b: bool| Some(Outcome::Value((b as u64).to_ne_bytes().to_vec()));
    match name {
        // PG's ExecEvalRowNullInt: a row with no fields is both NULL and NOT NULL
        "row nulltest ROW() IS NULL" | "row nulltest ROW() IS NOT NULL" | "row nulltest ROW(null, null) IS NULL" => boolean(true),
        "row nulltest ROW(1, 2) IS NOT NULL" | "row nulltest NULL::record IS NULL" | "rowcompare text 1 ABC DEF" => boolean(true),
        "row nulltest ROW(null, null) IS NOT NULL" | "row nulltest ROW(1, null) IS NULL" | "row nulltest ROW(1, null) IS NOT NULL" => boolean(false),
        "row nulltest ROW(1, 2) IS NULL" | "row nulltest NULL::record IS NOT NULL" => boolean(false),
        "rowcompare text 1 DEF ABC" | "rowcompare text 1 ABC ABC" => boolean(false),
        "greatest text" => text("abd"),
        "least text" => text("ab"),
        "format('%s', 'x')" => text("x"),
        "format('%s|%s', 'x', 7)" => text("x|7"),
        "format('%s', NULL)" => text(""),
        "format(NULL, 'x')" => Some(Outcome::Null),
        "concat(1, 'a')" => text("1a"),
        "concat(1, NULL)" => text("1"),
        "concat(x, 'a') per row" => text(if row == Some(&[Some(3)]) { "3a" } else { "a" }),
        "json_build_object('a', 1)" => text(r#"{"a" : 1}"#),
        "json_build_object('a', NULL)" => text(r#"{"a" : null}"#),
        _ => None,
    }
}

#[test]
fn differential() {
    install_seams();
    let cases = cases();
    let mut n = 0usize;
    let mut identical = 0usize;
    let mut same = [0usize; 3];
    let mut unsupported: BTreeMap<Kind, usize> = BTreeMap::new();
    let mut mismatches: Vec<String> = Vec::new();
    for case in &cases {
        let row_set: Vec<Option<Vec<Option<i32>>>> = if case.rows.is_empty() { vec![None] } else { case.rows.iter().cloned().map(Some).collect() };
        for row in row_set {
            n += 1;
            let ctx = MemoryContext::new("differential");
            let mcx = ctx.mcx();
            let expr = (case.build)(mcx);
            let typ = nodes_core::expr_type(expr);
            let (a, b) = {
                let mut bind_a = match &case.bind {
                    Bind::None => Bind::None,
                    Bind::Extern(v) => Bind::Extern(v.clone()),
                    Bind::Callback => Bind::Callback,
                    Bind::Exec(v) => Bind::Exec(v.clone()),
                };
                let mut bind_b = match &case.bind {
                    Bind::None => Bind::None,
                    Bind::Extern(v) => Bind::Extern(v.clone()),
                    Bind::Callback => Bind::Callback,
                    Bind::Exec(v) => Bind::Exec(v.clone()),
                };
                let a = run_incumbent(mcx, expr, bind_of(&mut bind_a), row.as_deref(), case.heap, typ, &case.aggs);
                let b = run_gravity(mcx, expr, bind_of(&mut bind_b), row.as_deref(), case.heap, typ, &case.aggs);
                (a, b)
            };
            if let Some(want) = pinned(&case.name, row.as_deref()) {
                if a != want || b != want {
                    mismatches.push(format!("{} row={row:?}: pinned {want:?}, incumbent {a:?}, gravity {b:?}", case.name));
                    continue;
                }
            }
            match (&a, &b) {
                (_, Outcome::Unsupported(k)) => *unsupported.entry(*k).or_default() += 1,
                (Outcome::Panic, _) | (_, Outcome::Panic) => mismatches.push(format!("{} row={row:?}: a panic is never a verdict: incumbent {a:?} vs gravity {b:?}", case.name)),
                _ if a == b => {
                    identical += 1;
                    same[match a {
                        Outcome::Value(_) => 0,
                        Outcome::Null => 1,
                        _ => 2,
                    }] += 1;
                }
                (Outcome::Value(x), Outcome::Value(y)) => {
                    let at = x.iter().zip(y.iter()).position(|(p, q)| p != q).unwrap_or(x.len().min(y.len()));
                    mismatches.push(format!("{} row={row:?}: values of {} / {} bytes differ from byte {at}: incumbent {:?} vs gravity {:?}", case.name, x.len(), y.len(), &x[at.saturating_sub(8)..(at + 24).min(x.len())], &y[at.saturating_sub(8)..(at + 24).min(y.len())]));
                }
                _ => mismatches.push(format!("{} row={row:?}: incumbent {a:?} vs gravity {b:?}", case.name)),
            }
        }
    }
    println!("differential: {n} cases, {identical} identical ({} values, {} nulls, {} errors), {} unsupported, {} mismatches", same[0], same[1], same[2], unsupported.values().sum::<usize>(), mismatches.len());
    for (k, c) in &unsupported {
        println!("  unsupported {k:?}: {c}");
    }
    for m in mismatches.iter().take(40) {
        println!("  MISMATCH {m}");
    }
    assert!(mismatches.is_empty(), "{} mismatches", mismatches.len());
    stable_bits();
    islands();
    store_emitted::run();
}

/// D901: `PgCatalog` with pg_proc's volatility for the listed functions (a unit run has no syscache: PgCatalog says None).
struct Vol(&'static [(Oid, u8)]);
impl<'m> gravity_expr::Catalog<'m> for Vol {
    fn func(&self, f: Oid) -> types_error::PgResult<gravity_expr::catalog::FuncFacts> {
        let mut facts = <PgCatalog as gravity_expr::Catalog<'m>>::func(&PgCatalog, f)?;
        facts.volatility = self.0.iter().find(|p| p.0 == f).map(|p| p.1);
        Ok(facts)
    }
    fn opcode(&self, o: Oid) -> types_error::PgResult<Oid> { <PgCatalog as gravity_expr::Catalog<'m>>::opcode(&PgCatalog, o) }
    fn op_input_types(&self, o: Oid) -> types_error::PgResult<(Oid, Oid)> { <PgCatalog as gravity_expr::Catalog<'m>>::op_input_types(&PgCatalog, o) }
    fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> types_error::PgResult<Oid> { <PgCatalog as gravity_expr::Catalog<'m>>::btree_cmp_proc(&PgCatalog, f, l, r) }
    fn opclass(&self, o: Oid) -> types_error::PgResult<(Oid, Oid)> { <PgCatalog as gravity_expr::Catalog<'m>>::opclass(&PgCatalog, o) }
    fn type_facts(&self, t: Oid) -> types_error::PgResult<gravity_expr::catalog::TypeFacts> { <PgCatalog as gravity_expr::Catalog<'m>>::type_facts(&PgCatalog, t) }
    fn type_cmp_proc(&self, t: Oid) -> types_error::PgResult<Oid> { <PgCatalog as gravity_expr::Catalog<'m>>::type_cmp_proc(&PgCatalog, t) }
    fn base_type(&self, t: Oid, m: &mut i32) -> types_error::PgResult<Oid> { <PgCatalog as gravity_expr::Catalog<'m>>::base_type(&PgCatalog, t, m) }
    fn domain(&self, t: Oid) -> types_error::PgResult<gravity_expr::catalog::DomainFacts<'m>> { <PgCatalog as gravity_expr::Catalog<'m>>::domain(&PgCatalog, t) }
    fn io(&self, t: Oid) -> types_error::PgResult<gravity_expr::catalog::IoFacts> { <PgCatalog as gravity_expr::Catalog<'m>>::io(&PgCatalog, t) }
    fn text(&self, c: Oid) -> types_error::PgResult<gravity_expr::catalog::TextFacts> { <PgCatalog as gravity_expr::Catalog<'m>>::text(&PgCatalog, c) }
    fn tier(&self, f: Oid) -> gravity_fn::Tier { <PgCatalog as gravity_expr::Catalog<'m>>::tier(&PgCatalog, f) }
    fn shim(&self, f: Oid) -> Option<&'static gravity_fn::Shim> { <PgCatalog as gravity_expr::Catalog<'m>>::shim(&PgCatalog, f) }
}

/// The coverage islands (nodes/mod.rs `covered`; M1-STEP, main 09-24): each node the emitter does not serve compiles to
/// an interpreter island at its root and passes `emit_check`; the same kinds the emitter serves stay emitted. Called from
/// `differential` (the seams are one thread's).
fn islands() {
    use SQLValueFunctionOp as Op;
    let of = |what: &str, build: &dyn for<'m> Fn(Mcx<'m>) -> Node<'m>| -> (bool, bool) {
        let ctx = MemoryContext::new("islands");
        let c = Expr::compile(lists(), build(ctx.mcx()), &PgCatalog, ParamBind::NONE).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        (c.prog.islands.iter().any(|i| i.root == c.root), c.emit_check().is_ok())
    };
    // (island at the root, emit_check passes)
    assert_eq!(of("a role SQLValueFunction", &|m| svf(m, Op::SVFOP_CURRENT_USER, 19, -1)), (true, true), "a role SQLValueFunction");
    assert_eq!(of("session_user", &|m| svf(m, Op::SVFOP_SESSION_USER, 19, -1)), (true, true), "session_user");
    assert_eq!(of("a datetime one is hosted calls", &|m| svf(m, Op::SVFOP_CURRENT_DATE, 1082, -1)), (false, true), "a datetime one is hosted calls");
    // a row-valued NullTest is emitted (emit/scalar.rs `row_null_test`), no island
    assert_eq!(of("a row-valued NullTest", &|m| null_test_row(m, row_expr(m, &[int4(m, Some(1)), int4(m, None)]), NullTestType::IS_NULL)), (false, true), "a row-valued NullTest");
    assert_eq!(of("a scalar NullTest", &|m| null_test(m, int4(m, None), NullTestType::IS_NULL)), (false, true), "a scalar NullTest");
    assert_eq!(of("a by-reference RowCompare", &|m| row_compare_text(m, 1, &[int4(m, Some(11)), text_const(m, "ABC", false)], &[int4(m, Some(11)), text_const(m, "DEF", false)])), (false, true), "a by-reference RowCompare");
    assert_eq!(of("an int4 RowCompare", &|m| row_compare(m, 1, &[int4(m, Some(1)), int4(m, Some(2))], &[int4(m, Some(1)), int4(m, Some(3))])), (false, true), "an int4 RowCompare");
    assert_eq!(of("a text GREATEST", &|m| minmax_text(m, false, &[text_const(m, "a", false), text_const(m, "b", false)])), (false, true), "a text GREATEST");
    assert_eq!(of("an int4 GREATEST", &|m| minmax(m, false, &[Some(1), Some(2)])), (false, true), "an int4 GREATEST");
    assert_eq!(of("an ArrayCoerceExpr", &|m| array_coerce(m, int4_array_const(m, &[Some(1), None]), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))), (true, true), "an ArrayCoerceExpr");
    // V4R-352 (M6-AGG): an extern parameter is an island's argument, after its columns: a word the plan binds per execution
    // (`param_needs`), handed to the interpreter by the call, never the compile's binding; V4R-454 (M8-EXPR): a PARAM_EXEC
    // (a nested loop's outer column, a SubPlan's argument) is one too, an Exec need
    fn coerce(m: Mcx<'_>, kind: ParamKind) -> Node<'_> {
        array_coerce(m, param(m, kind, 1, INT4ARRAYOID), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))
    }
    assert_eq!(of("an ArrayCoerceExpr over an extern parameter", &|m| coerce(m, ParamKind::PARAM_EXTERN)), (true, true), "an ArrayCoerceExpr over an extern parameter");
    assert_eq!(of("an ArrayCoerceExpr over a PARAM_EXEC", &|m| coerce(m, ParamKind::PARAM_EXEC)), (true, true), "an ArrayCoerceExpr over a PARAM_EXEC");
    // a relabel ArrayCoerce over a one-dimension ARRAY[...] (a varchar column's generic-plan IN ($1, $2)) is the constructor
    // of text elements (nodes/array.rs `compile_array_coerce`): emitted, no island, the parameters its needs in order; the
    // varchar(n) length check (a function) and a multidims constructor keep the ArrayCoerce, an island
    fn varchar_in(m: Mcx<'_>) -> Node<'_> {
        varchar_to_text(m, varchar_array_expr(m, &[param(m, ParamKind::PARAM_EXTERN, 1, VARCHAROID), param(m, ParamKind::PARAM_EXTERN, 2, VARCHAROID)], false))
    }
    assert_eq!(of("a relabel ArrayCoerce over ARRAY[$1, $2]", &|m| varchar_in(m)), (false, true), "a relabel ArrayCoerce over ARRAY[$1, $2]");
    assert_eq!(of("text = ANY (it)", &|m| text_any(m, text_const(m, "ab", false), varchar_in(m))), (false, true), "text = ANY (a relabel ArrayCoerce over ARRAY[$1, $2])");
    let fctx = MemoryContext::new("islands-fold");
    let c = Expr::compile(lists(), varchar_in(fctx.mcx()), &PgCatalog, ParamBind::NONE).expect("compiles");
    assert!(c.kinds().all(|k| k != Kind::ArrayCoerceExpr), "no ArrayCoerce left");
    assert!(matches!(&c.prog.nodes[c.root.0 as usize], gravity_expr::nodes::ExprNode::Array(a) if a.array_type == 1009 && a.elem.typ == TEXTOID && !a.multidims), "the root is the text[] constructor");
    let needs = c.param_needs(scratch()).expect("needs");
    assert!(matches!(needs[..], [gravity_expr::emit::ParamNeed::Extern { id: 1, typ: VARCHAROID }, gravity_expr::emit::ParamNeed::Extern { id: 2, typ: VARCHAROID }]), "$1, $2: {needs:?}");
    fn n2(m: Mcx<'_>) -> Node<'_> {
        varchar_array_expr(m, &[param(m, ParamKind::PARAM_EXTERN, 1, VARCHAROID), param(m, ParamKind::PARAM_EXTERN, 2, VARCHAROID)], false)
    }
    assert_eq!(of("a varchar(2) ArrayCoerce over ARRAY[$1, $2]", &|m| varchar_to_varchar_n(m, n2(m), 2, false)), (true, true), "a length check keeps the ArrayCoerce");
    assert_eq!(of("a relabel ArrayCoerce over a multidims ARRAY[...]", &|m| varchar_to_text(m, varchar_array_expr(m, &[n2(m), n2(m)], true))), (true, true), "a multidims constructor keeps the ArrayCoerce");
    let ctx = MemoryContext::new("islands-param");
    let c = Expr::compile(lists(), coerce(ctx.mcx(), ParamKind::PARAM_EXTERN), &PgCatalog, ParamBind::NONE).expect("compiles");
    let isl = c.prog.islands.iter().find(|i| i.root == c.root).expect("an island at the root");
    let args = c.prog.sites[isl.site.0 as usize].args;
    let arg = &c.prog.nodes[c.prog.kids[args.start as usize].0 as usize];
    assert!(args.len == 1 && matches!(arg, gravity_expr::nodes::ExprNode::Param(q) if q.id == 1), "the island's one argument is $1 ({} arguments)", args.len);
    let needs = c.param_needs(scratch()).expect("needs");
    assert!(matches!(needs[..], [gravity_expr::emit::ParamNeed::Extern { id: 1, typ: INT4ARRAYOID }]), "one Extern need: {needs:?}");
    // V4R-454: an extern and an executor parameter of one id are two arguments, one Extern and one Exec need, in order
    fn both(m: Mcx<'_>) -> Node<'_> {
        let ps = [param(m, ParamKind::PARAM_EXTERN, 1, INT4ARRAYOID), param(m, ParamKind::PARAM_EXEC, 1, INT4ARRAYOID)];
        let co = Node::mk(m, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4ARRAYOID, coalescecollid: 0, args: NodeList::from_slice(m, &ps).unwrap(), location: -1 }).unwrap();
        array_coerce(m, co, 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))
    }
    let c = Expr::compile(lists(), both(ctx.mcx()), &PgCatalog, ParamBind::NONE).expect("compiles");
    let isl = c.prog.islands.iter().find(|i| i.root == c.root).expect("an island at the root");
    assert_eq!(c.prog.sites[isl.site.0 as usize].args.len, 2, "$1 and exec param 1 are two island arguments");
    let needs = c.param_needs(scratch()).expect("needs");
    assert!(matches!(needs[..], [gravity_expr::emit::ParamNeed::Extern { id: 1, .. }, gravity_expr::emit::ParamNeed::Exec { id: 1, .. }]), "one Extern then one Exec need: {needs:?}");
    // the same $1 read twice under one island is one argument and one need (a word per parameter id, not per read)
    fn twice(m: Mcx<'_>) -> Node<'_> {
        let ps = [param(m, ParamKind::PARAM_EXTERN, 1, INT4ARRAYOID), param(m, ParamKind::PARAM_EXTERN, 1, INT4ARRAYOID)];
        let co = Node::mk(m, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4ARRAYOID, coalescecollid: 0, args: NodeList::from_slice(m, &ps).unwrap(), location: -1 }).unwrap();
        array_coerce(m, co, 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))
    }
    let c = Expr::compile(lists(), twice(ctx.mcx()), &PgCatalog, ParamBind::NONE).expect("compiles");
    let isl = c.prog.islands.iter().find(|i| i.root == c.root).expect("an island at the root");
    assert_eq!(c.prog.sites[isl.site.0 as usize].args.len, 1, "$1 read twice is one island argument");
    let needs = c.param_needs(scratch()).expect("needs");
    assert!(matches!(needs[..], [gravity_expr::emit::ParamNeed::Extern { id: 1, typ: INT4ARRAYOID }]), "$1 read twice is one Extern need: {needs:?}");
    // D901 (main 07:39Z): an island over extern parameters alone whose every function is a built-in IMMUTABLE one is `Once`
    // (computed once per distinct argument values, emit/host.rs); a STABLE function, a column read, or a catalog that cannot
    // say (a unit run has no syscache: PgCatalog's volatility is None) keeps it per row. `Vol` reports pg_proc's volatility.
    fn over_col(m: Mcx<'_>) -> Node<'_> {
        array_coerce(m, scan_var(m, 1, INT4ARRAYOID), 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))
    }
    fn once_of(cat: &dyn for<'m> Fn(Mcx<'m>, Node<'m>) -> Option<(u32, bool)>, e: fn(Mcx<'_>) -> Node<'_>) -> Option<(u32, bool)> {
        let ctx = MemoryContext::new("islands-once");
        cat(ctx.mcx(), e(ctx.mcx()))
    }
    fn with<'m>(cat: &dyn gravity_expr::Catalog<'m>, n: Node<'m>) -> Option<(u32, bool)> {
        let c = Expr::compile(lists(), n, cat, ParamBind::NONE).expect("compiles");
        let isl = *c.prog.islands.iter().find(|i| i.root == c.root).expect("an island at the root");
        isl.once.map(|o| (o.varlena, o.ret_byval))
    }
    fn extern_coerce(m: Mcx<'_>) -> Node<'_> {
        coerce(m, ParamKind::PARAM_EXTERN)
    }
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i')]), n), extern_coerce), Some((1, false)), "int4[] $1 -> int8[] over int8(int4), IMMUTABLE: Once, its varlena argument and result");
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b's')]), n), extern_coerce), None, "a STABLE element function stays per row");
    assert_eq!(once_of(&|_, n| with(&PgCatalog, n), extern_coerce), None, "no catalog volatility (a unit run): per row, fail closed");
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i')]), n), over_col), None, "an island reading a column is per row");
    // the argument masks are u32 (bit i: argument i): 32 extern parameters are Once, 33 (a site carries up to MAX_ARGS) stay
    // per row
    fn wide(m: Mcx<'_>, n: i32) -> Node<'_> {
        let ps: Vec<Node<'_>> = (1..=n).map(|id| param(m, ParamKind::PARAM_EXTERN, id, INT4ARRAYOID)).collect();
        let co = Node::mk(m, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4ARRAYOID, coalescecollid: 0, args: NodeList::from_slice(m, &ps).unwrap(), location: -1 }).unwrap();
        array_coerce(m, co, 1016, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])))
    }
    fn wide32(m: Mcx<'_>) -> Node<'_> {
        wide(m, 32)
    }
    fn wide33(m: Mcx<'_>) -> Node<'_> {
        wide(m, 33)
    }
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i')]), n), wide32), Some((u32::MAX, false)), "32 extern parameters: Once, every argument a varlena");
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i')]), n), wide33), None, "33 extern parameters (past the u32 argument masks) stay per row");
    // a nested island (an array coercion over an array-coercion island, as PG reads `c = ANY($1::varchar(2)[])`: main
    // 08:56Z) is Once when every function in both is a built-in IMMUTABLE one; either one's missing facts keep it per row
    fn nested_of(m: Mcx<'_>, kind: ParamKind) -> Node<'_> {
        let inner = array_coerce(m, param(m, kind, 1, INT4ARRAYOID), INT8ARRAYOID, Some(funcexpr(m, 481, INT8OID, 0, &[case_test_of(m, INT4OID)])));
        array_coerce(m, inner, INT4ARRAYOID, Some(funcexpr(m, 480, INT4OID, 0, &[case_test_of(m, INT8OID)])))
    }
    fn nested(m: Mcx<'_>) -> Node<'_> {
        nested_of(m, ParamKind::PARAM_EXTERN)
    }
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i'), (480, b'i')]), n), nested), Some((1, false)), "int4[] $1 -> int8[] -> int4[]: the outer island is Once over the inner one");
    assert_eq!(once_of(&|_, n| with(&Vol(&[(481, b'i')]), n), nested), None, "the outer element function without volatility facts: per row");
    assert_eq!(once_of(&|_, n| with(&Vol(&[(480, b'i')]), n), nested), None, "the inner island's function without volatility facts: per row");
    store_emitted::once(coerce, &[(481, b'i')], INT8ARRAYOID);
    store_emitted::once(nested_of, &[(481, b'i'), (480, b'i')], INT4ARRAYOID);
    store_emitted::folded();
}

/// `CallFacts.stable`: an unknown-typed literal's bit only; a typed literal, a Var and a NULL unknown literal
/// are stable too in C's sense but a cached program is re-bound with other words — only the unknown literal's
/// cstring conversion (`extract_variadic_args`) may read the flag. Called from `differential`: the seams'
/// `Once` and the session timezone are one thread's.
fn stable_bits() {
    let stable_of = |build: &dyn for<'m> Fn(Mcx<'m>) -> Node<'m>| -> Vec<u128> {
        let ctx = MemoryContext::new("stable");
        let mcx = ctx.mcx();
        let c = Expr::compile(lists(), build(mcx), &PgCatalog, ParamBind::NONE).expect("compiles");
        c.state.call_facts.iter().map(|f| f.stable).collect()
    };
    assert_eq!(stable_of(&|m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[text_const(m, "%s", false), unknown_const(m, Some("x"))])), vec![0b10]);
    assert_eq!(stable_of(&|m| funcexpr(m, 3539, TEXTOID, C_COLLATION, &[text_const(m, "%s", false), text_const(m, "x", false)])), vec![0]);
    assert_eq!(stable_of(&|m| funcexpr(m, 3058, TEXTOID, C_COLLATION, &[int4(m, Some(1)), unknown_const(m, Some("a"))])), vec![0b10]);
    assert_eq!(stable_of(&|m| funcexpr(m, 3200, JSONOID, 0, &[unknown_const(m, Some("a")), int4(m, Some(1))])), vec![0b01]);
    assert_eq!(stable_of(&|m| funcexpr(m, 3200, JSONOID, 0, &[unknown_const(m, Some("a")), unknown_const(m, None), unknown_const(m, Some("b")), int4(m, Some(2))])), vec![0b0111]);
}

// ---------------------------------------------------------------------------------------------
// The write-side forms emitted (card E-a, emit/store.rs; lane M4-EXPR): the SubscriptingRef store, FieldStore and
// NextValueExpr through the incumbent (execexpr, the PG 18.6 port), the gravity interpreter and the emitted program (the
// IR interpreter and TPDE), row by row: value, null, error (the first cell's, else the raise mask's). nextval's calls
// are counted per engine and must be equal (one per row PG evaluates it on). Called from `differential` (the seams are
// one thread's).
// ---------------------------------------------------------------------------------------------
mod store_emitted {
    use super::*;
    use gravity_abi::status::raise;
    use gravity_expr::emit::{ColsRef, EmitState, ParamNeed, RowRefs};
    use gravity_expr::Compiled;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{ConvOp, Type};
    use types_rel::{FormData_pg_class, LockInfoData, LockRelId, RelationData, RELKIND_COMPOSITE_TYPE};

    const A: (Oid, Oid, Oid) = (INT4ARRAYOID, INT4OID, INT4ARRAYOID);
    const J: (Oid, Oid, Oid) = (JSONBOID, JSONBOID, JSONBOID);
    const CA: (Oid, Oid, Oid) = (COMP_ARRAY_OID, COMP_OID, COMP_ARRAY_OID);

    fn comp_desc(mcx: Mcx<'_>) -> TupleDescData<'_> {
        let mut attrs = PgVec::new_in(mcx);
        let mut compact = PgVec::new_in(mcx);
        for (i, (name, typ, len, byval)) in [("x", INT4OID, 4i16, true), ("arr", INT4ARRAYOID, -1, false)].into_iter().enumerate() {
            let mut attname = types_tuple::NameData::default();
            attname.namestrcpy(name);
            let att = FormData_pg_attribute { attrelid: COMP_REL, attname, attnum: (i + 1) as i16, atttypid: typ, atttypmod: -1, attlen: len, attbyval: byval, attalign: TYPALIGN_INT, attstorage: if byval { TYPSTORAGE_PLAIN } else { b'x' as i8 }, ..Default::default() };
            compact.push(CompactAttribute::populate_from(&att));
            attrs.push(att);
        }
        TupleDescData { natts: 2, tdtypeid: COMP_OID, tdtypmod: -1, tdrefcount: -1, constr: None, compact_attrs: compact, attrs }
    }

    /// The composite type's relation (`relation_open(COMP_REL)`: the typcache's descriptor of `comp`).
    pub(super) fn comp_relation(mcx: Mcx<'_>) -> RelationData<'_> {
        let mut relname = types_tuple::NameData::default();
        relname.namestrcpy("comp");
        let rd_rel = FormData_pg_class { relname, relnamespace: 2200, reltype: COMP_OID, relowner: 10, relam: 0, relfilenode: 0, reltablespace: 0, relpages: 0, reltuples: -1.0, relallvisible: 0, reltoastrelid: 0, relhasindex: false, relisshared: false, relpersistence: types_core::RELPERSISTENCE_PERMANENT, relkind: RELKIND_COMPOSITE_TYPE, relhassubclass: false, relrowsecurity: false, relispopulated: true, relreplident: b'n', relispartition: false, relfrozenxid: 0, relminmxid: 0 };
        RelationData {
            rd_locator: Default::default(),
            rd_smgr: Default::default(),
            rd_id: COMP_REL,
            rd_backend: types_core::INVALID_PROC_NUMBER,
            rd_islocaltemp: false,
            rd_isvalid: std::cell::Cell::new(true),
            rd_createSubid: std::cell::Cell::new(0),
            rd_newRelfilelocatorSubid: std::cell::Cell::new(0),
            rd_firstRelfilelocatorSubid: std::cell::Cell::new(0),
            rd_droppedSubid: std::cell::Cell::new(0),
            rd_lockInfo: LockInfoData { lockRelId: LockRelId { relId: COMP_REL, dbId: 5 } },
            rd_rel,
            rd_att: std::rc::Rc::new(comp_desc(mcx)),
            rd_index: None,
            rd_opcintype: PgVec::new_in(mcx),
            rd_opfamily: PgVec::new_in(mcx),
            rd_indoption: PgVec::new_in(mcx),
            rd_indcollation: PgVec::new_in(mcx),
            rd_options: None,
            pgstat_enabled: std::cell::Cell::new(false),
            pgstat_link: std::cell::Cell::new((0, core::ptr::null_mut())),
            rd_amcache: Default::default(),
            rd_amcache_hash: Default::default(),
            rd_amcache_gin: Default::default(),
            rd_amcache_spgist: Default::default(),
            rd_amcache_bloom: Default::default(),
            rd_support: PgVec::new_in(mcx),
            rd_supportinfo: Default::default(),
            rd_opcoptions: Default::default(),
            rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: false,
            rd_hasrules: false,
        }
    }

    fn int4_array_datum(mcx: Mcx<'_>, elems: &[Option<i32>]) -> Datum {
        let values: Vec<Datum> = elems.iter().map(|v| v.map_or(Datum::null(), Datum::from_i32)).collect();
        let nulls: Vec<bool> = elems.iter().map(|v| v.is_none()).collect();
        let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &[elems.len() as i32], &[1], INT4OID, 4, true, b'i').unwrap();
        Datum::from_usize(img.leak().as_ptr() as usize)
    }

    /// A `comp` constant `(x, arr)`.
    fn comp_const<'m>(mcx: Mcx<'m>, x: Option<i32>, arr: Option<&[Option<i32>]>) -> Node<'m> {
        let desc = comp_desc(mcx);
        let vals = [x.map_or(Datum::null(), Datum::from_i32), arr.map_or(Datum::null(), |a| int4_array_datum(mcx, a))];
        let nulls = [x.is_none(), arr.is_none()];
        let mut tuple = heaptuple::heap_form_tuple(mcx, &desc, &vals, &nulls).unwrap();
        let len = tuple.image().len() as u32;
        let hdr = tuple.as_tuple_mut().t_data_mut();
        hdr.set_datum_length(len);
        hdr.set_type_id(COMP_OID);
        hdr.set_typmod(-1);
        let d = Datum::from_usize(tuple.as_tuple().header_ptr() as usize);
        core::mem::forget(tuple);
        Node::mk_const(mcx, COMP_OID, -1, 0, -1, d, false, false).unwrap()
    }

    fn fstore<'m>(mcx: Mcx<'m>, arg: Node<'m>, typ: Oid, fields: &[(i32, Node<'m>)]) -> Node<'m> {
        let mut nums = types_nodes::list::IntList::nil();
        let mut vals = Vec::new();
        for &(f, v) in fields {
            nums.lappend(mcx, f).unwrap();
            vals.push(v);
        }
        Node::mk(mcx, types_nodes::primnodes::FieldStore { arg, newvals: NodeList::from_slice(mcx, &vals).unwrap(), fieldnums: nums, resulttype: typ }).unwrap()
    }

    fn nextval(mcx: Mcx<'_>, typ: Oid) -> Node<'_> {
        Node::mk(mcx, types_nodes::primnodes::NextValueExpr { seqid: 7, typeId: typ }).unwrap()
    }

    fn v(m: Mcx<'_>, k: i16) -> Node<'_> {
        scan_var(m, k, INT4OID)
    }
    fn i(m: Mcx<'_>, x: i32) -> Node<'_> {
        int4(m, Some(x))
    }
    fn div<'m>(m: Mcx<'m>, a: Node<'m>, b: Node<'m>) -> Node<'m> {
        opexpr(m, 154, INT4OID, 0, &[a, b])
    }
    fn null_of_type(m: Mcx<'_>, typ: Oid) -> Node<'_> {
        konst(m, typ, -1, false, None)
    }
    /// `CaseTest[k] := x` (the rewriter's element store under an indirection).
    fn el<'m>(m: Mcx<'m>, k: Node<'m>, x: Node<'m>) -> Node<'m> {
        sbsref(m, case_test_of(m, INT4ARRAYOID), &[Some(k)], &[], Some(x), A)
    }

    struct SCase {
        name: &'static str,
        build: Build,
        ext: Vec<ParamExternData>,
        /// A write-side form (a store / nextval site in the program); false = the does-not-apply shape (G4).
        store: bool,
    }

    fn sc(name: &'static str, build: impl for<'m> Fn(Mcx<'m>) -> Node<'m> + 'static) -> SCase {
        SCase { name, build: Box::new(build), ext: Vec::new(), store: true }
    }

    fn cases() -> Vec<SCase> {
        vec![
            sc("store element", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2), Some(3)]), &[Some(v(m, 1))], &[], Some(v(m, 2)), A)),
            sc("store element into a null array", |m| sbsref(m, null_of_type(m, INT4ARRAYOID), &[Some(v(m, 1))], &[], Some(v(m, 2)), A)),
            sc("store element into an empty array", |m| sbsref(m, int4_array_const(m, &[]), &[Some(v(m, 1))], &[], Some(v(m, 2)), A)),
            sc("store: a null subscript raises before the value", |m| sbsref(m, int4_array_const(m, &[Some(1)]), &[Some(v(m, 1))], &[], Some(div(m, v(m, 2), i(m, 0))), A)),
            sc("store slice", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2), Some(3), Some(4)]), &[Some(opexpr(m, 177, INT4OID, 0, &[v(m, 1), i(m, 1)]))], &[Some(v(m, 1))], Some(int4_array_const(m, &[Some(9), Some(8)])), A)),
            sc("store slice, open upper", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2), Some(3), Some(4)]), &[None], &[Some(v(m, 1))], Some(int4_array_const(m, &[Some(7), Some(7), Some(7)])), A)),
            sc("store slice, open lower", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2), Some(3)]), &[Some(v(m, 1))], &[None], Some(int4_array_const(m, &[Some(6), Some(6), Some(6)])), A)),
            sc("store slice, null source", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2)]), &[Some(v(m, 1))], &[Some(v(m, 1))], Some(null_of_type(m, INT4ARRAYOID)), A)),
            sc("store slice into a null array", |m| sbsref(m, null_of_type(m, INT4ARRAYOID), &[Some(i(m, 2))], &[Some(i(m, 1))], Some(array_expr(m, &[v(m, 2), v(m, 1)])), A)),
            sc("store 2-D element", |m| sbsref(m, array_expr_md(m, &[array_expr(m, &[i(m, 1), i(m, 2)]), array_expr(m, &[i(m, 3), i(m, 4)])]), &[Some(v(m, 1)), Some(i(m, 2))], &[], Some(v(m, 2)), A)),
            sc("store 2-D slice", |m| sbsref(m, array_expr_md(m, &[array_expr(m, &[i(m, 1), i(m, 2)]), array_expr(m, &[i(m, 3), i(m, 4)])]), &[Some(i(m, 2)), Some(v(m, 1))], &[Some(i(m, 2)), Some(i(m, 1))], Some(array_expr_md(m, &[array_expr(m, &[v(m, 2), v(m, 2)])])), A)),
            SCase { ext: vec![ParamExternData { value: Datum::from_i32(42), isnull: false, pflags: PARAM_FLAG_CONST, ptype: INT4OID }], ..sc("store element from a param", |m| sbsref(m, int4_array_const(m, &[Some(1)]), &[Some(v(m, 1))], &[], Some(param(m, ParamKind::PARAM_EXTERN, 1, INT4OID)), A)) },
            sc("jsonb store by index", |m| sbsref(m, jsonb_const(m, "[1, 2, 3]"), &[Some(v(m, 1))], &[], Some(jsonb_const(m, "\"x\"")), J)),
            sc("jsonb store by key into null", |m| sbsref(m, null_of_type(m, JSONBOID), &[Some(text_const(m, "k", false))], &[], Some(jsonb_const(m, "{\"a\": [1]}")), J)),
            sc("jsonb store a null value", |m| sbsref(m, jsonb_const(m, "{\"a\": 1}"), &[Some(text_const(m, "a", false))], &[], Some(null_of_type(m, JSONBOID)), J)),
            sc("jsonb store, null index", |m| sbsref(m, jsonb_const(m, "[1]"), &[Some(v(m, 1))], &[], Some(jsonb_const(m, "2")), J)),
            sc("field store", |m| fstore(m, comp_const(m, Some(1), Some(&[Some(1), Some(2)])), COMP_OID, &[(1, v(m, 1))])),
            sc("field store two fields over a null composite", |m| fstore(m, null_of_type(m, COMP_OID), COMP_OID, &[(1, v(m, 1)), (2, array_expr(m, &[v(m, 2)]))])),
            sc("field store nested element", |m| fstore(m, comp_const(m, Some(1), Some(&[Some(1), Some(2)])), COMP_OID, &[(2, el(m, v(m, 1), v(m, 2)))])),
            sc("field store the same field twice", |m| fstore(m, comp_const(m, Some(1), Some(&[Some(1)])), COMP_OID, &[(2, el(m, i(m, 1), v(m, 1))), (2, el(m, i(m, 2), v(m, 2)))])),
            sc("field store nested into a null composite", |m| fstore(m, null_of_type(m, COMP_OID), COMP_OID, &[(1, v(m, 2)), (2, el(m, v(m, 1), v(m, 2)))])),
            sc("field store: a raise stops the later values", |m| fstore(m, comp_const(m, Some(1), None), COMP_OID, &[(1, div(m, i(m, 10), v(m, 1))), (2, el(m, v(m, 1), v(m, 2)))])),
            sc("array of composite: a nested field", |m| sbsref(m, null_of_type(m, COMP_ARRAY_OID), &[Some(i(m, 1))], &[], Some(fstore(m, case_test_of(m, COMP_OID), COMP_OID, &[(1, v(m, 1))])), CA)),
            sc("array of composite: two nested stores (f[1].a[1], f[1].a[2])", |m| {
                let inner = sbsref(m, null_of_type(m, COMP_ARRAY_OID), &[Some(i(m, 1))], &[], Some(fstore(m, case_test_of(m, COMP_OID), COMP_OID, &[(2, el(m, i(m, 1), v(m, 1)))])), CA);
                sbsref(m, inner, &[Some(i(m, 1))], &[], Some(fstore(m, case_test_of(m, COMP_OID), COMP_OID, &[(2, el(m, i(m, 2), v(m, 2)))])), CA)
            }),
            sc("array of composite: the old element through a relabel", |m| {
                let inner = sbsref(m, null_of_type(m, COMP_ARRAY_OID), &[Some(i(m, 1))], &[], Some(fstore(m, case_test_of(m, COMP_OID), COMP_OID, &[(1, v(m, 1))])), CA);
                sbsref(m, inner, &[Some(i(m, 1))], &[], Some(relabel(m, fstore(m, case_test_of(m, COMP_OID), COMP_OID, &[(2, el(m, i(m, 1), v(m, 2)))]), COMP_OID)), CA)
            }),
            sc("domain over an array", |m| domain_coercion(m, sbsref(m, int4_array_const(m, &[Some(5), Some(6)]), &[Some(v(m, 1))], &[], Some(v(m, 2)), A), ARRDOM_OID)),
            sc("domain over an array, two stores", |m| {
                let first = domain_coercion(m, sbsref(m, null_of_type(m, INT4ARRAYOID), &[Some(i(m, 1))], &[], Some(v(m, 1)), A), ARRDOM_OID);
                domain_coercion(m, sbsref(m, first, &[Some(i(m, 2))], &[], Some(v(m, 2)), A), ARRDOM_OID)
            }),
            sc("nextval int4", |m| nextval(m, INT4OID)),
            sc("nextval int2", |m| nextval(m, INT2OID)),
            sc("nextval int8", |m| nextval(m, INT8OID)),
            sc("nextval is not called after a raise", |m| fstore(m, comp_const(m, Some(1), None), COMP_OID, &[(1, div(m, i(m, 10), v(m, 1))), (1, nextval(m, INT4OID))])),
            sc("nextval in a store's value", |m| sbsref(m, int4_array_const(m, &[Some(1)]), &[Some(v(m, 1))], &[], Some(nextval(m, INT4OID)), A)),
            // G4 (does not apply): a fetch keeps the fetch site; no store site, no volatile site
            SCase { store: false, ..sc("a fetch is not a store", |m| sbsref(m, int4_array_const(m, &[Some(1), Some(2)]), &[Some(v(m, 1))], &[], None, (INT4ARRAYOID, INT4OID, INT4OID))) },
        ]
    }

    const ROWS: &[[Option<i32>; 2]] = &[[Some(1), Some(10)], [Some(5), Some(20)], [None, Some(30)], [Some(0), None], [Some(-2), Some(7)], [Some(3), Some(3)], [Some(2), Some(-1)], [Some(1), Some(0)]];

    #[repr(C)]
    struct Frame {
        values: *const u64,
        nulls: *const u8,
        consts: *const u64,
        consts_null: *const u8,
        region: *mut Region,
        cells: *mut gravity_expr::emit::Cell,
        out_val: *mut u64,
        out_valid: *mut u8,
        out_err: *mut u64,
    }

    /// One row: the constants loaded, `emit` over the scan row, the three outputs stored.
    fn program(c: &Compiled<'_>, nconsts: usize) -> (gravity_ir::Program, gravity_ir::Func) {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("store_emitted", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let ctx = b.params()[0];
        let values = b.load(Type::Ptr, ctx, 0);
        let nulls = b.load(Type::Ptr, ctx, 8);
        let cp = b.load(Type::Ptr, ctx, 16);
        let cnp = b.load(Type::Ptr, ctx, 24);
        let region = b.load(Type::Ptr, ctx, 32);
        let cells = b.load(Type::Ptr, ctx, 40);
        let out_val = b.load(Type::Ptr, ctx, 48);
        let out_valid = b.load(Type::Ptr, ctx, 56);
        let out_err = b.load(Type::Ptr, ctx, 64);
        let zero = b.iconst(Type::I64, 0);
        let mut consts = Vec::new();
        let mut consts_null = Vec::new();
        for j in 0..nconsts {
            consts.push(b.load(Type::I64, cp, (j * 8) as i32));
            let n = b.load_zext(Type::I8, cnp, j as i32);
            consts_null.push(b.icmp(gravity_ir::IntCond::Ne, n, zero));
        }
        let row = RowRefs { scratch: scratch(), scan: ColsRef::Slot { values, nulls }, inner: ColsRef::None, outer: ColsRef::None, consts: &consts, consts_null: &consts_null, region: Some(region), cells: Some(cells), shreds: &[], codes: &[], tables: &[], rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[] };
        let l = c.emit(&mut b, &row).unwrap_or_else(|e| panic!("emit: {e:?}"));
        b.store(out_val, 0, l.value);
        let valid8 = b.conv(ConvOp::Zext, Type::I8, l.valid);
        b.store(out_valid, 0, valid8);
        b.store(out_err, 0, l.err);
        b.ret(&[zero]);
        drop(b);
        (pb.finish(), f)
    }

    /// The emitted program over every row through `code`: the outcomes and nextval's calls.
    fn run_code(code: gravity_program::Code, state: &mut EmitState, consts: &[u64], consts_null: &[u8], typ: Oid) -> (Vec<Outcome>, u64) {
        NEXTVAL_CALLS.with(|c| c.set(0));
        let mut out = Vec::new();
        for row in ROWS {
            let values: Vec<u64> = row.iter().map(|x| x.map_or(0, |x| Datum::from_i32(x).as_u64())).collect();
            let nulls: Vec<u8> = row.iter().map(|x| x.is_none() as u8).collect();
            let mut rbuf = vec![0u8; 1 << 20];
            let mut region = Region::over(&mut rbuf);
            let (mut val, mut valid, mut err) = (0u64, 0u8, 0u64);
            let mut frame = Frame { values: values.as_ptr(), nulls: nulls.as_ptr(), consts: consts.as_ptr(), consts_null: consts_null.as_ptr(), region: &mut region, cells: state.ptr(), out_val: &mut val, out_valid: &mut valid, out_err: &mut err };
            // SAFETY: the frame is the ctx the function reads; every pointer is live for the call.
            let r = unsafe { code.call((&mut frame as *mut Frame).cast()) };
            assert_eq!(r, 0);
            // the row's first cell error (the lowerer's `h_raise` order), every other one dropped
            let mut first = None;
            for s in 0..state.len() {
                if let Some(e) = state.take_error(s) {
                    first.get_or_insert(e);
                }
            }
            out.push(if err != 0 {
                match first {
                    Some(e) => err_of(&e),
                    None => {
                        let (s, m) = raise::describe(raise::first(err)).expect("a described raise");
                        Outcome::Error(types_error::make_sqlstate(*s), m.to_string())
                    }
                }
            } else if valid == 0 {
                Outcome::Null
            } else {
                bytes_of(NullableDatum { value: Datum::from_u64(val), isnull: false }, typ)
            });
        }
        (out, NEXTVAL_CALLS.with(|c| c.get()))
    }

    /// D901: the `Once` island `build(PARAM_EXTERN)` (an int4[] $1 coerced to `typ`, its functions IMMUTABLE by `vol`: int8[]
    /// over int8(int4), and the nested int4[] -> int8[] -> int4[]) emitted and run
    /// over every row, one emit state across bindings as a re-executed prepared statement's: the incumbent's value on each
    /// row, one evaluation per binding (a changed $1 misses, the same one again misses once more: one value is kept), a
    /// NULL $1 too.
    pub(super) fn once(build: fn(Mcx<'_>, ParamKind) -> Node<'_>, vol: &'static [(Oid, u8)], typ: Oid) {
        let ctx = MemoryContext::new("islands-once-run");
        let mcx = ctx.mcx();
        let expr = build(mcx, ParamKind::PARAM_EXTERN);
        let mut c = Expr::compile(lists(), expr, &Vol(vol), ParamBind::NONE).expect("compiles");
        assert!(c.prog.islands.iter().any(|i| i.root == c.root && i.once.is_some()), "a Once island at the root");
        let needs = c.param_needs(scratch()).unwrap();
        let (ir, f) = program(&c, needs.len());
        gravity_ir::verify::verify(&ir).unwrap();
        let mut state = c.emit_state(emit_cx());
        let bindings: [Option<&[Option<i32>]>; 4] = [Some(&[Some(1), None, Some(3)]), Some(&[Some(7)]), Some(&[Some(1), None, Some(3)]), None];
        for (k, b) in bindings.iter().enumerate() {
            let ext = vec![ParamExternData { value: b.map_or(Datum::null(), |a| int4_array_datum(mcx, a)), isnull: b.is_none(), pflags: PARAM_FLAG_CONST, ptype: INT4ARRAYOID }];
            let want: Vec<Outcome> = ROWS.iter().map(|r| run_incumbent(mcx, expr, ParamBind { extern_params: Some(ext.as_slice()), ..ParamBind::NONE }, Some(r), false, typ, &[])).collect();
            let consts: Vec<u64> = needs.iter().map(|_| ext[0].value.as_u64()).collect();
            let consts_null: Vec<u8> = needs.iter().map(|_| ext[0].isnull as u8).collect();
            let before = state.islands.iter().filter_map(|h| h.once.as_ref()).map(|o| o.evals()).sum::<u64>();
            let code = gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("tpde: {}", e.message()));
            let (got, _) = run_code(code, &mut state, &consts, &consts_null, typ);
            let evals = state.islands.iter().filter_map(|h| h.once.as_ref()).map(|o| o.evals()).sum::<u64>() - before;
            assert_eq!(got, want, "binding {k} ({b:?}): the incumbent's value on every row");
            assert_eq!(evals, 1, "binding {k} ({b:?}): one evaluation over {} rows", ROWS.len());
        }
    }

    /// The folded relabel ArrayCoerce (`islands`: ARRAY[$1, $2, $3] of varchar to text[], and `'ab' = ANY` of it under
    /// texteq) emitted and run on TPDE and the IR interpreter: the incumbent's value on every row of every binding, NULL
    /// parameters included, one emit state across bindings as a re-executed generic plan's.
    pub(super) fn folded() {
        fn arr(m: Mcx<'_>) -> Node<'_> {
            let ps: Vec<Node<'_>> = (1..=3).map(|id| param(m, ParamKind::PARAM_EXTERN, id, VARCHAROID)).collect();
            varchar_to_text(m, varchar_array_expr(m, &ps, false))
        }
        fn any(m: Mcx<'_>) -> Node<'_> {
            text_any(m, text_const(m, "ab", false), arr(m))
        }
        fn varlena(mcx: Mcx<'_>, s: &str) -> Datum {
            let mut img: PgVec<u8> = mcx::vec_with_capacity_in(mcx, s.len() + 4).unwrap();
            mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + 4)).unwrap();
            mcx::vec_append_bytes(&mut img, s.as_bytes()).unwrap();
            Datum::from_usize(img.leak().as_ptr() as usize)
        }
        let bindings: [[Option<&str>; 3]; 5] = [[Some("ab"), Some("c"), Some("")], [Some("x"), None, Some("ab")], [None, None, None], [Some("x"), None, Some("y")], [Some(""), Some("AB"), Some("ab ")]];
        for (what, build, typ) in [("ARRAY[$1, $2, $3]::text[]", arr as fn(Mcx<'_>) -> Node<'_>, 1009), ("'ab' = ANY (ARRAY[$1, $2, $3]::text[])", any as fn(Mcx<'_>) -> Node<'_>, BOOLOID)] {
            let ctx = MemoryContext::new("coerce-fold");
            let mcx = ctx.mcx();
            let expr = build(mcx);
            let mut c = Expr::compile(lists(), expr, &PgCatalog, ParamBind::NONE).expect("compiles");
            assert!(c.prog.islands.is_empty(), "{what}: no island");
            c.emit_check().unwrap_or_else(|e| panic!("{what}: emit_check: {e:?}"));
            let needs = c.param_needs(scratch()).unwrap();
            let (ir, f) = program(&c, needs.len());
            gravity_ir::verify::verify(&ir).unwrap();
            let mut state = c.emit_state(emit_cx());
            for (k, b) in bindings.iter().enumerate() {
                let ext: Vec<ParamExternData> = b.iter().map(|s| ParamExternData { value: s.map_or(Datum::null(), |s| varlena(mcx, s)), isnull: s.is_none(), pflags: PARAM_FLAG_CONST, ptype: VARCHAROID }).collect();
                let want: Vec<Outcome> = ROWS.iter().map(|r| run_incumbent(mcx, expr, ParamBind { extern_params: Some(ext.as_slice()), ..ParamBind::NONE }, Some(r), false, typ, &[])).collect();
                let mut consts = Vec::new();
                let mut consts_null = Vec::new();
                for n in &needs {
                    let d = match n {
                        ParamNeed::Const(d) | ParamNeed::Elem(d) => *d,
                        ParamNeed::Extern { id, .. } => NullableDatum { value: ext[*id as usize - 1].value, isnull: ext[*id as usize - 1].isnull },
                        ParamNeed::Exec { .. } => panic!("{what}: an executor parameter"),
                        ParamNeed::Deferred { .. } => panic!("{what}: a deferred parameter (D2834: no PL list here)"),
                    };
                    consts.push(d.value.as_u64());
                    consts_null.push(d.isnull as u8);
                }
                let tpde = run_code(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{what}: tpde: {}", e.message())), &mut state, &consts, &consts_null, typ);
                assert_eq!(tpde.0, want, "{what}: binding {k} ({b:?}): TPDE vs the incumbent");
                let interp = run_code(gravity_backend::interp(ir.clone(), f).unwrap(), &mut state, &consts, &consts_null, typ);
                assert_eq!(interp.0, want, "{what}: binding {k} ({b:?}): the IR interpreter vs the incumbent");
            }
        }
    }

    pub(super) fn run() {
        let mut mismatches: Vec<String> = Vec::new();
        for case in cases() {
            let ctx = MemoryContext::new("store_emitted");
            let mcx = ctx.mcx();
            let expr = (case.build)(mcx);
            let typ = nodes_core::expr_type(expr);
            let bind = || if case.ext.is_empty() { ParamBind::NONE } else { ParamBind { extern_params: Some(case.ext.as_slice()), ..ParamBind::NONE } };
            let mut engines: Vec<(&str, Vec<Outcome>, u64)> = Vec::new();
            for (name, incumbent) in [("incumbent", true), ("gravity", false)] {
                NEXTVAL_CALLS.with(|c| c.set(0));
                let outs = ROWS.iter().map(|r| if incumbent { run_incumbent(mcx, expr, bind(), Some(r), false, typ, &[]) } else { run_gravity(mcx, expr, bind(), Some(r), false, typ, &[]) }).collect();
                engines.push((name, outs, NEXTVAL_CALLS.with(|c| c.get())));
            }
            let mut c = Expr::compile(lists(), expr, &PgCatalog, bind()).unwrap_or_else(|e| panic!("{}: compile: {e:?}", case.name));
            c.emit_check().unwrap_or_else(|e| panic!("{}: emit_check: {e:?}", case.name));
            assert!(c.prog.islands.is_empty(), "{}: an island (the store forms are host sites, never the interpreter's)", case.name);
            let names: Vec<&str> = c.prog.sites.iter().map(|s| gravity_expr::nodes::hostcall::of(s).name).collect();
            let has_store = names.iter().any(|n| matches!(*n, "subscript store" | "field store")) || c.prog.sites.iter().any(|s| s.volatile);
            assert_eq!(has_store, case.store, "{}: sites {names:?}", case.name);
            let needs = c.param_needs(scratch()).unwrap();
            let (mut consts, mut consts_null) = (Vec::new(), Vec::new());
            for n in &needs {
                let d = match n {
                    ParamNeed::Const(d) | ParamNeed::Elem(d) => *d,
                    ParamNeed::Extern { id, .. } => NullableDatum { value: case.ext[*id as usize - 1].value, isnull: case.ext[*id as usize - 1].isnull },
                    ParamNeed::Exec { .. } | ParamNeed::Deferred { .. } => NullableDatum::null(),
                };
                consts.push(d.value.as_u64());
                consts_null.push(d.isnull as u8);
            }
            let (ir, f) = program(&c, needs.len());
            gravity_ir::verify::verify(&ir).unwrap_or_else(|e| panic!("{}: verify: {e:?}", case.name));
            let mut state = c.emit_state(emit_cx());
            let tpde = run_code(gravity_backend::mapped(&ir, f).unwrap_or_else(|e| panic!("{}: tpde: {}", case.name, e.message())), &mut state, &consts, &consts_null, typ);
            engines.push(("emitted/tpde", tpde.0, tpde.1));
            let interp = run_code(gravity_backend::interp(ir, f).unwrap(), &mut state, &consts, &consts_null, typ);
            engines.push(("emitted/interp", interp.0, interp.1));
            let (_, want, want_calls) = &engines[0];
            for (name, got, calls) in &engines[1..] {
                for (k, (w, g)) in want.iter().zip(got.iter()).enumerate() {
                    if w != g {
                        mismatches.push(format!("{} row={:?}: incumbent {w:?} vs {name} {g:?}", case.name, ROWS[k]));
                    }
                }
                if calls != want_calls {
                    mismatches.push(format!("{}: nextval calls: incumbent {want_calls} vs {name} {calls}", case.name));
                }
            }
            let brief: Vec<String> = want.iter().map(|o| match o {
                Outcome::Value(b) => format!("V{}", b.len()),
                Outcome::Null => "N".into(),
                Outcome::Error(_, m) => format!("E({m})"),
                other => format!("{other:?}"),
            }).collect();
            println!("store {}: calls {want_calls} {brief:?}", case.name);
            if case.name.starts_with("nextval int") {
                assert_eq!(*want_calls, ROWS.len() as u64, "{}: one call per row", case.name);
            }
        }
        for m in &mismatches {
            println!("  STORE MISMATCH {m}");
        }
        assert!(mismatches.is_empty(), "{} store mismatches", mismatches.len());
    }
}


/// A context for the tests' cells (leaked: they outlive the call that made them).
fn emit_cx() -> mcx::Mcx<'static> {
    Box::leak(Box::new(mcx::MemoryContext::new("emit test"))).mcx()
}
