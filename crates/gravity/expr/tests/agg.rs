//! The aggregate library (expr-compiler.md §4.6): the native transition / combine / final forms
//! over the builtin families against hand-computed results (numeric through `numeric_out`), the
//! combine of two halves equal to the whole, and the refusals by name.

use std::sync::Once;

use datum::Datum;
use gravity_expr::agg::{Agg, AggPlan, AggState, Final, Transition};
use gravity_expr::{NullableDatum, PgCatalog, Region};
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::Aggref;
use types_tuple::PgTypeShape;

/// memsig E-prog: this thread's test context for compiled programs' lists (never freed: a test compiles a few).
fn lists() -> mcx::Mcx<'static> {
    thread_local! { static CX: &'static mcx::MemoryContext = Box::leak(Box::new(mcx::MemoryContext::new("test program lists"))); } // alloc: Cold
    CX.with(|c| *c).mcx()
}

const INT4OID: Oid = 23;
const INT8OID: Oid = 20;
const FLOAT4OID: Oid = 700;
const FLOAT8OID: Oid = 701;
const BOOLOID: Oid = 16;
const TEXTOID: Oid = 25;
const NUMERICOID: Oid = 1700;
const INT4ARRAYOID: Oid = 1007;
const C_COLLATION: Oid = 950;

static SEAMS: Once = Once::new();

fn shape(typlen: i16, typbyval: bool, typalign: u8, typcollation: Oid) -> PgTypeShape {
    PgTypeShape { typlen, typbyval, typalign: typalign as i8, typstorage: if typlen == -1 { b'x' as i8 } else { b'p' as i8 }, typcollation }
}

fn agg_shape(transfn: Oid, transtype: Oid) -> syscache_seams::PgAggregateShape {
    syscache_seams::PgAggregateShape { aggkind: b'n' as i8, aggnumdirectargs: 0, aggtransfn: transfn, aggfinalfn: 0, aggcombinefn: transfn, aggserialfn: 0, aggdeserialfn: 0, aggmtransfn: 0, aggminvtransfn: 0, aggmfinalfn: 0, aggfinalextra: false, aggmfinalextra: false, aggfinalmodify: b'r' as i8, aggmfinalmodify: b'r' as i8, aggsortop: 0, aggtranstype: transtype, aggtransspace: 0, aggmtranstype: 0 }
}

fn install_seams() {
    SEAMS.call_once(|| {
        syscache_seams::lookup_pg_type_shape::set(|typid| {
            Ok(match typid {
                INT4OID => Some(shape(4, true, b'i', 0)),
                INT8OID => Some(shape(8, true, b'd', 0)),
                FLOAT4OID => Some(shape(4, true, b'i', 0)),
                FLOAT8OID => Some(shape(8, true, b'd', 0)),
                BOOLOID => Some(shape(1, true, b'c', 0)),
                TEXTOID => Some(shape(-1, false, b'i', 100)),
                NUMERICOID | INT4ARRAYOID => Some(shape(-1, false, b'i', 0)),
                _ => None,
            })
        });
        syscache_seams::pg_type_element_shape::set(|typid| Ok((typid == INT4ARRAYOID).then_some(syscache_seams::PgTypeElementShape { typelem: INT4OID, typsubscript: lsyscache::F_ARRAY_SUBSCRIPT_HANDLER })));
        syscache_seams::pg_type_typnamespace::set(|_| Ok(Some(11)));
        syscache_seams::pg_type_typtype::set(|_| Ok(Some(b'b' as i8)));
        syscache_seams::lookup_pg_proc_shape::set(|_| Ok(None));
        syscache_seams::lookup_pg_aggregate_shape::set(|aggfnoid| {
            Ok(match aggfnoid {
                2116 => Some(agg_shape(768, INT4OID)),
                2132 => Some(agg_shape(769, INT4OID)),
                2145 => Some(agg_shape(459, TEXTOID)),
                2130 => Some(agg_shape(1767, NUMERICOID)),
                _ => None,
            })
        });
    });
}

fn aggref<'m>(m: Mcx<'m>, aggfnoid: Oid, args: &[Oid], aggtype: Oid, kind: u8) -> &'m Aggref<'m> {
    let mut a = Node::build::<Aggref>(m).unwrap();
    a.aggfnoid = aggfnoid;
    a.aggtype = aggtype;
    a.aggkind = kind as i8;
    a.inputcollid = C_COLLATION;
    for &t in args {
        a.aggargtypes.lappend(m, t).unwrap();
    }
    a.seal().as_aggref().unwrap()
}

fn numeric(s: &str) -> Vec<u8> {
    let mut call = gravity_fn::FmgrCall::bind(1701, 3, 0).unwrap();
    let mut c = s.as_bytes().to_vec();
    c.push(0);
    let v = call.call_site(&[Datum::from_usize(c.as_ptr() as usize), Datum::from_u32(0), Datum::from_i32(-1)], &[false; 3]).unwrap();
    let p = v.value.as_usize() as *const u8;
    unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }.to_vec()
}

fn numeric_str(d: NullableDatum) -> String {
    assert!(!d.isnull);
    let p = d.value.as_usize() as *const u8;
    let img = unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) };
    let mut out = Vec::new();
    adt_numeric::numeric_out_into(adt_numeric::Num::from_payload(&img[4..]), &mut out);
    String::from_utf8(out).unwrap()
}

fn text(s: &str) -> Vec<u8> {
    let mut v = datum::set_varsize_4b(s.len() + 4).to_vec();
    v.extend_from_slice(s.as_bytes());
    v
}

fn payload(d: NullableDatum) -> Vec<u8> {
    assert!(!d.isnull);
    let p = d.value.as_usize() as *const u8;
    unsafe { core::slice::from_raw_parts(p.add(4), types_tuple::varatt::varsize_4b(p) - 4) }.to_vec()
}

fn dat(v: &[u8]) -> Datum {
    Datum::from_usize(v.as_ptr() as usize)
}

/// Run `rows` (each a full argument list) through the plan: transition per row, then the final.
fn run(plan: &mut AggPlan, rows: &[Vec<NullableDatum>], region: &mut Region) -> NullableDatum {
    let mut st = plan.init();
    for r in rows {
        let args: Vec<Datum> = r.iter().map(|d| d.value).collect();
        let nulls: Vec<bool> = r.iter().map(|d| d.isnull).collect();
        plan.transition(&mut st, &args, &nulls, region).unwrap();
    }
    plan.finalize(&st, region).unwrap()
}

fn run_split(plan: &mut AggPlan, rows: &[Vec<NullableDatum>], at: usize, region: &mut Region) -> NullableDatum {
    let mut a = plan.init();
    let mut b = plan.init();
    for (i, r) in rows.iter().enumerate() {
        let args: Vec<Datum> = r.iter().map(|d| d.value).collect();
        let nulls: Vec<bool> = r.iter().map(|d| d.isnull).collect();
        plan.transition(if i < at { &mut a } else { &mut b }, &args, &nulls, region).unwrap();
    }
    plan.combine(&mut a, &b, region).unwrap();
    plan.finalize(&a, region).unwrap()
}

fn i4(v: Option<i32>) -> Vec<NullableDatum> {
    vec![v.map_or(NullableDatum::null(), |v| NullableDatum::value(Datum::from_i32(v)))]
}

fn i8(v: Option<i64>) -> Vec<NullableDatum> {
    vec![v.map_or(NullableDatum::null(), |v| NullableDatum::value(Datum::from_i64(v)))]
}

fn f8(v: f64) -> Vec<NullableDatum> {
    vec![NullableDatum::value(Datum::from_f64(v))]
}

fn nd(v: Option<&[u8]>) -> NullableDatum {
    v.map_or(NullableDatum::null(), |v| NullableDatum::value(dat(v)))
}

#[test]
fn native_forms() {
    install_seams();
    let ctx = MemoryContext::new("agg");
    let m = ctx.mcx();
    let mut buf = vec![0u8; 1 << 20];
    let mut region = Region::over(&mut buf);
    let r = &mut region;

    let mut p = Agg::compile(aggref(m, 2803, &[], INT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::CountStar, Final::Identity));
    assert_eq!(run(&mut p, &[vec![], vec![], vec![]], r).value.as_i64(), 3);
    assert_eq!(run(&mut p, &[], r).value.as_i64(), 0);
    let mut p = Agg::compile(aggref(m, 2147, &[INT4OID], INT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(run(&mut p, &[i4(Some(1)), i4(None), i4(Some(2))], r).value.as_i64(), 2);

    let mut p = Agg::compile(aggref(m, 2108, &[INT4OID], INT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(p.transition, Transition::SumInt);
    let rows = [i4(Some(1)), i4(Some(2)), i4(None), i4(Some(i32::MAX))];
    assert_eq!(run(&mut p, &rows, r).value.as_i64(), 3 + i32::MAX as i64);
    assert!(run(&mut p, &[i4(None)], r).isnull);
    assert_eq!(run_split(&mut p, &rows, 2, r), run(&mut p, &rows, r));

    let mut p = Agg::compile(aggref(m, 2107, &[INT8OID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::SumInt128, Final::Int128Sum));
    let rows = [i8(Some(i64::MAX)), i8(Some(i64::MAX)), i8(None)];
    assert_eq!(numeric_str(run(&mut p, &rows, r)), "18446744073709551614");
    assert_eq!(numeric_str(run_split(&mut p, &rows, 1, r)), "18446744073709551614");
    assert_eq!(numeric_str(run(&mut p, &[i8(Some(-5))], r)), "-5");
    assert!(run(&mut p, &[i8(None)], r).isnull);
    let mut p = Agg::compile(aggref(m, 2100, &[INT8OID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!(numeric_str(run(&mut p, &[i8(Some(1)), i8(Some(2)), i8(Some(4))], r)), "2.3333333333333333");

    let mut p = Agg::compile(aggref(m, 2101, &[INT4OID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::AvgInt, Final::IntAvg));
    let rows = [i4(Some(1)), i4(None), i4(Some(2))];
    assert_eq!(numeric_str(run(&mut p, &rows, r)), "1.5000000000000000");
    assert_eq!(numeric_str(run_split(&mut p, &rows, 1, r)), "1.5000000000000000");
    assert!(run(&mut p, &[i4(None)], r).isnull);

    let mut p = Agg::compile(aggref(m, 2111, &[FLOAT8OID], FLOAT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(run(&mut p, &[f8(1.5), f8(2.25)], r).value.as_f64(), 3.75);
    let e = { let mut st = p.init(); p.transition(&mut st, &[Datum::from_f64(f64::MAX)], &[false], r).unwrap(); p.transition(&mut st, &[Datum::from_f64(f64::MAX)], &[false], r).unwrap_err() };
    assert_eq!(e.message(), "value out of range: overflow");
    let mut p = Agg::compile(aggref(m, 2105, &[FLOAT8OID], FLOAT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::AvgF64, Final::F64Avg));
    let rows = [f8(1.5), f8(2.25), f8(0.0)];
    assert_eq!(run(&mut p, &rows, r).value.as_f64(), 1.25);
    assert_eq!(run_split(&mut p, &rows, 2, r).value.as_f64(), 1.25);
    assert!(run(&mut p, &[], r).isnull);
    // D244: float8_accum's Sxx overflows from finite inputs though Sx stays finite (PG raises); a combine of the two halves
    // raises at float8_combine's Sxx check; NaN / Infinity propagate without an error
    let step_err = |p: &mut AggPlan, r: &mut Region, xs: &[f64]| {
        let mut st = p.init();
        xs.iter().try_for_each(|x| p.transition(&mut st, &[Datum::from_f64(*x)], &[false], r)).unwrap_err().message().to_string()
    };
    assert_eq!(step_err(&mut p, r, &[1e200, -1e200]), "value out of range: overflow");
    let (mut a, mut b) = (p.init(), p.init());
    p.transition(&mut a, &[Datum::from_f64(1e200)], &[false], r).unwrap();
    p.transition(&mut b, &[Datum::from_f64(-1e200)], &[false], r).unwrap();
    assert_eq!(p.combine(&mut a, &b, r).unwrap_err().message(), "value out of range: overflow");
    assert!(run(&mut p, &[f8(f64::INFINITY), f8(1.0)], r).value.as_f64() == f64::INFINITY);
    assert!(run(&mut p, &[f8(f64::INFINITY), f8(f64::NEG_INFINITY)], r).value.as_f64().is_nan());
    assert!(run(&mut p, &[f8(f64::NAN), f8(1.0)], r).value.as_f64().is_nan());
    assert!(run_split(&mut p, &[f8(f64::INFINITY), f8(1.0)], 1, r).value.as_f64() == f64::INFINITY);
    let mut p = Agg::compile(aggref(m, 2104, &[FLOAT4OID], FLOAT8OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(run(&mut p, &[vec![NullableDatum::value(Datum::from_f32(1.0))], vec![NullableDatum::value(Datum::from_f32(2.0))]], r).value.as_f64(), 1.5);

    let (a, b, c) = (numeric("1.10"), numeric("2.205"), numeric("NaN"));
    let mut p = Agg::compile(aggref(m, 2114, &[NUMERICOID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!(p.transition, Transition::SumNumeric);
    let rows = [vec![nd(Some(&a))], vec![nd(None)], vec![nd(Some(&b))]];
    assert_eq!(numeric_str(run(&mut p, &rows, r)), "3.305");
    assert_eq!(numeric_str(run_split(&mut p, &rows, 1, r)), "3.305");
    assert_eq!(numeric_str(run(&mut p, &[vec![nd(Some(&a))], vec![nd(Some(&c))]], r)), "NaN");
    assert!(run(&mut p, &[vec![nd(None)]], r).isnull);
    let mut p = Agg::compile(aggref(m, 2103, &[NUMERICOID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!(p.final_, Final::NumericAvg);
    let (one, two) = (numeric("1"), numeric("2"));
    assert_eq!(numeric_str(run(&mut p, &[vec![nd(Some(&one))], vec![nd(Some(&two))]], r)), "1.5000000000000000");

    let mut p = Agg::compile(aggref(m, 2116, &[INT4OID], INT4OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(p.transition, Transition::MinMax);
    let rows = [i4(Some(3)), i4(Some(9)), i4(None), i4(Some(1))];
    assert_eq!(run(&mut p, &rows, r).value.as_i32(), 9);
    assert_eq!(run_split(&mut p, &rows, 1, r).value.as_i32(), 9);
    assert!(run(&mut p, &[i4(None)], r).isnull);
    let mut p = Agg::compile(aggref(m, 2132, &[INT4OID], INT4OID, b'n'), &PgCatalog).unwrap();
    assert_eq!(run(&mut p, &rows, r).value.as_i32(), 1);
    let (ta, tb, tc) = (text("b"), text("a"), text("c"));
    let mut p = Agg::compile(aggref(m, 2145, &[TEXTOID], TEXTOID, b'n'), &PgCatalog).unwrap();
    let rows = [vec![nd(Some(&ta))], vec![nd(Some(&tb))], vec![nd(Some(&tc))]];
    assert_eq!(payload(run(&mut p, &rows, r)), b"a");
    assert_eq!(payload(run_split(&mut p, &rows, 2, r)), b"a");
    let mut p = Agg::compile(aggref(m, 2130, &[NUMERICOID], NUMERICOID, b'n'), &PgCatalog).unwrap();
    assert_eq!(numeric_str(run(&mut p, &[vec![nd(Some(&a))], vec![nd(Some(&b))]], r)), "2.205");

    let t = |b: bool| vec![NullableDatum::value(Datum::from_bool(b))];
    let mut p = Agg::compile(aggref(m, 2517, &[BOOLOID], BOOLOID, b'n'), &PgCatalog).unwrap();
    assert!(!run(&mut p, &[t(true), t(false)], r).value.as_bool());
    assert!(run(&mut p, &[t(true), t(true)], r).value.as_bool());
    assert!(run(&mut p, &[], r).isnull);
    let mut p = Agg::compile(aggref(m, 2518, &[BOOLOID], BOOLOID, b'n'), &PgCatalog).unwrap();
    assert!(run(&mut p, &[t(false), t(true)], r).value.as_bool());
    assert!(!run_split(&mut p, &[t(false), t(false)], 1, r).value.as_bool());

    let (comma, x, y, z) = (text(","), text("x"), text("y"), text("z"));
    let mut p = Agg::compile(aggref(m, 3538, &[TEXTOID, TEXTOID], TEXTOID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::StringAgg, Final::Text));
    let rows = [vec![nd(Some(&x)), nd(Some(&comma))], vec![nd(None), nd(Some(&comma))], vec![nd(Some(&y)), nd(None)], vec![nd(Some(&z)), nd(Some(&comma))]];
    assert_eq!(payload(run(&mut p, &rows, r)), b"xy,z");
    assert_eq!(payload(run_split(&mut p, &rows, 2, r)), b"xy,z");
    assert!(run(&mut p, &[vec![nd(None), nd(Some(&comma))]], r).isnull);
    let long: Vec<Vec<NullableDatum>> = (0..2000).map(|_| vec![nd(Some(&x)), nd(Some(&comma))]).collect();
    assert_eq!(payload(run(&mut p, &long, r)).len(), 2000 * 2 - 1);

    let mut p = Agg::compile(aggref(m, 2335, &[INT4OID], INT4ARRAYOID, b'n'), &PgCatalog).unwrap();
    assert_eq!((p.transition, p.final_), (Transition::ArrayAgg, Final::Array));
    let rows = [i4(Some(1)), i4(None), i4(Some(3))];
    let out = run(&mut p, &rows, r);
    let img = unsafe { core::slice::from_raw_parts(out.value.as_usize() as *const u8, types_tuple::varatt::varsize_4b(out.value.as_usize() as *const u8)) };
    assert_eq!((arrayfuncs::arr_ndim(img), arrayfuncs::arr_dim(img, 0), arrayfuncs::arr_hasnull(img)), (1, 3, true));
    let (vals, nulls) = arrayfuncs::deconstruct_array(m, img, 4, true, b'i', true).unwrap();
    assert_eq!((vals[0].as_i32(), nulls[1], vals[2].as_i32()), (1, true, 3));
    assert_eq!(payload(run_split(&mut p, &rows, 1, r)), payload(out));
    assert!(run(&mut p, &[], r).isnull);
}

#[test]
fn refused_by_name() {
    install_seams();
    let ctx = MemoryContext::new("agg");
    let m = ctx.mcx();
    let e = Agg::compile(aggref(m, 3972, &[FLOAT8OID], FLOAT8OID, b'o'), &PgCatalog).err().unwrap();
    assert!(e.to_string().contains("ordered-set"), "{e}");
    let e = Agg::compile(aggref(m, 2119, &[FLOAT4OID], FLOAT4OID, b'n'), &PgCatalog).err().unwrap();
    assert!(e.to_string().contains("no pg_aggregate row"), "{e}");
    let st = AggState::EMPTY;
    assert!(st.value.isnull);
}

/// lowerv4 M3 decision 15 (D346): an ORDER BY inside the call refuses by name unless the v4 Builder flag lifts the gate
/// (`Agg::compile_with(.., true)`).
#[test]
fn the_aggorder_gate_lifts_only_under_the_flag() {
    install_seams();
    let ctx = MemoryContext::new("agg aggorder");
    let m = ctx.mcx();
    let ordered = || {
        let order = types_nodes::list::NodeList::make1(m, Node::mk(m, types_nodes::parsenodes::SortGroupClause::default()).unwrap()).unwrap();
        let mut a = Node::build::<Aggref>(m).unwrap();
        (a.aggfnoid, a.aggtype, a.aggkind, a.inputcollid) = (2116, INT4OID, b'n' as i8, C_COLLATION); // max(int4)
        a.aggargtypes.lappend(m, INT4OID).unwrap();
        a.aggorder = order;
        a.seal().as_aggref().unwrap()
    };
    assert!(Agg::compile(ordered(), &PgCatalog).is_err(), "the gate stands by default");
    assert!(Agg::compile_with(ordered(), &PgCatalog, false).is_err());
    assert!(Agg::compile_with(ordered(), &PgCatalog, true).is_ok(), "the flag lifts the ORDER BY gate");
}

/// M6-FOLD R2 (the Builder's `folded`): a lowerer that folds every aggregate itself compiles an Aggref as its cell
/// after `Agg::cell_gate` alone. Under `folded` no `AggPlan` is built: an aggregate no plan can bind (no pg_aggregate
/// row here, as a non-strict or context-bound transfn has no Generic plan) still compiles, to a node that reads the
/// row's cell `aggno` and nothing else; without the flag the same Aggref refuses by name. The cell gates stand under both,
/// except an ordered-set / hypothetical-set call's, which `folded` lets through to lowerv4's class.ordered (V4R-425).
#[test]
fn a_folded_builder_compiles_an_aggregate_as_its_cell_and_never_its_plan() {
    install_seams();
    let ctx = MemoryContext::new("agg folded");
    let m = ctx.mcx();
    let call = |fnoid: Oid, kind: u8| {
        let mut a = Node::build::<Aggref>(m).unwrap();
        (a.aggfnoid, a.aggtype, a.aggkind, a.inputcollid, a.aggno) = (fnoid, NUMERICOID, kind as i8, C_COLLATION, 1);
        a.aggargtypes.lappend(m, INT4OID).unwrap();
        a.seal()
    };
    let compile = |n: Node<'_>, folded: bool| {
        let (mut prog, mut state) = (gravity_expr::Program::empty_in(lists()), gravity_expr::State::empty());
        let r = gravity_expr::nodes::Builder { prog: &mut prog, state: &mut state, catalog: &PgCatalog, outer: None, tableoid: None, aggorder: false, returning: None, folded }.compile(n);
        r.map(|root| (match &prog.nodes[root.0 as usize] { gravity_expr::ExprNode::Aggref(a) => Some(a.aggno), _ => None }, prog.naggs))
    };
    for fnoid in [2149, 2119] {
        // variance(int4) (int4_accum: non-strict, an internal state) and one without a pg_aggregate row
        let e = compile(call(fnoid, b'n'), false).err().expect("no plan: refused without the flag");
        assert!(e.to_string().contains("no pg_aggregate row"), "{e}");
        let (cell, naggs) = compile(call(fnoid, b'n'), true).expect("the cell compiles under folded");
        assert_eq!(cell, Some(1), "the root is a read of cell 1, no other node");
        assert_eq!(naggs, 2, "cells 0..=1 bound to the row");
    }
    // V4R-425: an ordered-set call (percentile_cont, no DISTINCT) passes `folded_gate` to its cell (lowerv4's class.ordered
    // folds it or refuses it by name); without the flag the cell gate still refuses it by name
    let (cell, naggs) = compile(call(3972, b'o'), true).expect("an ordered-set cell compiles under folded");
    assert_eq!((cell, naggs), (Some(1), 2), "the root is a read of cell 1");
    let e = compile(call(3972, b'o'), false).err().expect("the cell gate stands without the flag");
    assert!(e.to_string().contains("ordered-set"), "{e}");
}
