//! D4562 pin: btcostestimate's outputs for a fixed set of btree index paths, equal to the last bit to what trunk
//! e59aa10685f computes (the build before D4562 rewrote btcostestimate to read its path in place). A cost-estimator
//! refactor can change plans silently, and PG regress no longer guards EXPLAIN output, so this pins them exactly.
//!
//! Four fixture tables (a, b, c int4; 1,000 pages, 100,000 tuples, real-looking pg_statistic rows), each with one
//! btree index: (a); (a, b, c); (b) WHERE c > 0 (partial); ((a + b)) (expression, with its own statistics). Each case
//! plans one query, walks the base rel's surviving paths (an IndexPath, or the IndexPaths under a bitmap heap path),
//! and records per IndexPath the path's rows / startup / total, indextotalcost and indexselectivity, plus
//! amcostestimate's five outputs (startup, total, selectivity, correlation, pages) at loop_count 1 and 25. Floats
//! print with `{:?}` (the shortest form that reads back to the same bits), so equal lines = equal values.
//!
//! Range constants sit inside the histograms' middle buckets: an end bucket makes the planner read the index's actual
//! endpoint (get_actual_variable_range), which needs storage these fixtures do not have. That endpoint path is
//! deliberately NOT pinned here; the e2e and regress suites cover it.
//!
//! On a mismatch the panic prints the actual lines as Rust literals; GOLDEN was captured that way from e59aa10685f.
use super::*;
use types_nodes::primnodes::{BoolExpr, BoolExprType, OpExpr, ScalarArrayOpExpr};
use types_pathnodes::PathNode;

const PIN_T1: u32 = 16430;
const PIN_I1: u32 = 16431;
const PIN_T2: u32 = 16432;
const PIN_I2: u32 = 16433;
const PIN_T3: u32 = 16434;
const PIN_I3: u32 = 16435;
const PIN_T4: u32 = 16436;
const PIN_I4: u32 = 16437;
const INT4PL_OP: u32 = 551;
const INT4PL_PROC: u32 = 177;
const INT4LT_PROC: u32 = 66;
const INT4GT_PROC: u32 = 147;
const HEAP_PAGES: u32 = 1000;
const HEAP_TUPLES: f32 = 100_000.0;

/// One index per table: (oid, name, key columns (0 = the expression), partial, pages, tuples, tree height).
struct PinIndex {
    oid: u32,
    name: &'static str,
    indkey: &'static [i16],
    partial: bool,
    pages: u32,
    tuples: f32,
    height: u32,
}

fn pin_index(table: u32) -> Option<PinIndex> {
    let (oid, name, indkey, partial, pages, tuples, height) = match table {
        PIN_T1 => (PIN_I1, "pin1_a", &[1i16][..], false, 276, HEAP_TUPLES, 1),
        PIN_T2 => (PIN_I2, "pin2_abc", &[1i16, 2, 3][..], false, 386, HEAP_TUPLES, 2),
        PIN_T3 => (PIN_I3, "pin3_b_part", &[2i16][..], true, 139, 50_000.0, 1),
        PIN_T4 => (PIN_I4, "pin4_expr", &[0i16][..], false, 276, HEAP_TUPLES, 1),
        _ => return None,
    };
    Some(PinIndex { oid, name, indkey, partial, pages, tuples, height })
}

fn table_of_index(index: u32) -> Option<u32> {
    [PIN_T1, PIN_T2, PIN_T3, PIN_T4].into_iter().find(|&t| pin_index(t).is_some_and(|i| i.oid == index))
}

fn pin_pg_class(oid: u32) -> Option<types_rel::FormData_pg_class> {
    let mut form = if let Some(t) = pin_index(oid) {
        let mut f = make_pg_class(oid, &format!("pin{}", t.oid - 1), b'r', 2, true);
        f.relpages = HEAP_PAGES as i32;
        f.reltuples = HEAP_TUPLES;
        f
    } else {
        let t = pin_index(table_of_index(oid)?)?;
        let mut f = make_pg_class(oid, t.name, b'i', 403, false);
        f.relpages = t.pages as i32;
        f.reltuples = t.tuples;
        f
    };
    form.relallvisible = 0;
    Some(form)
}

fn var<'mcx>(mcx: Mcx<'mcx>, attno: i16) -> Node<'mcx> {
    Node::mk_var(mcx, 1, attno, 23, -1, 0, 0).unwrap()
}

fn int4<'mcx>(mcx: Mcx<'mcx>, v: i32) -> Node<'mcx> {
    Node::mk_const(mcx, 23, -1, 0, 4, Datum::from_i32(v), false, true).unwrap()
}

fn op<'mcx>(mcx: Mcx<'mcx>, opno: u32, opfuncid: u32, restype: u32, l: Node<'mcx>, r: Node<'mcx>) -> Node<'mcx> {
    Node::mk(
        mcx,
        OpExpr {
            opno,
            opfuncid,
            opresulttype: restype,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, l, r).unwrap(),
            location: -1,
        },
    )
    .unwrap()
}

fn a_plus_b<'mcx>(mcx: Mcx<'mcx>) -> Node<'mcx> {
    op(mcx, INT4PL_OP, INT4PL_PROC, 23, var(mcx, 1), var(mcx, 2))
}

fn and<'mcx>(mcx: Mcx<'mcx>, l: Node<'mcx>, r: Node<'mcx>) -> Node<'mcx> {
    Node::mk(mcx, BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(mcx, l, r).unwrap(), location: -1 })
        .unwrap()
}

fn int4_array<'mcx>(mcx: Mcx<'mcx>, elems: &[i32]) -> Node<'mcx> {
    let total = 24 + 4 * elems.len();
    let mut image: mcx::PgVec<'_, u8> = mcx::vec_with_capacity_in(mcx, total).unwrap();
    image.extend_from_slice(&datum::varlena::set_varsize_4b(total));
    image.extend_from_slice(&1i32.to_ne_bytes());
    image.extend_from_slice(&0i32.to_ne_bytes());
    image.extend_from_slice(&23i32.to_ne_bytes());
    image.extend_from_slice(&(elems.len() as i32).to_ne_bytes());
    image.extend_from_slice(&1i32.to_ne_bytes());
    for e in elems {
        image.extend_from_slice(&e.to_ne_bytes());
    }
    let value = Datum::from_usize(image.leak().as_ptr() as usize);
    Node::mk_const(mcx, 1007, -1, 0, -1, value, false, false).unwrap()
}

/// relation_open for the pin tables and their indexes (None = not a pin relation).
pub(super) fn relation<'mcx>(mcx: Mcx<'mcx>, relid: u32) -> Option<types_rel::Relation<'mcx>> {
    use types_tuple::tupdesc::ATTNULLABLE_UNRESTRICTED;
    let form = pin_pg_class(relid)?;
    let attrs_of = |names: &[&str]| {
        let mut attrs = mcx::PgVec::new_in(mcx);
        for (i, n) in names.iter().enumerate() {
            attrs.push(int4_attr(i as i16 + 1, n, false));
        }
        let mut compact_attrs = mcx::PgVec::new_in(mcx);
        for a in attrs.iter() {
            let mut c = types_tuple::CompactAttribute::populate_from(a);
            c.attnullability = ATTNULLABLE_UNRESTRICTED;
            compact_attrs.push(c);
        }
        std::rc::Rc::new(types_tuple::TupleDescData {
            natts: names.len() as i32,
            tdtypeid: 0,
            tdtypmod: -1,
            tdrefcount: 1,
            constr: None,
            compact_attrs,
            attrs,
        })
    };
    if pin_index(relid).is_some() {
        return Some(types_rel::Relation::open(make_rel_data(mcx, relid, form, attrs_of(&["a", "b", "c"][..])), None));
    }
    let table = table_of_index(relid)?;
    let t = pin_index(table)?;
    let names: Vec<&str> = t.indkey.iter().map(|&k| ["expr", "a", "b", "c"][k as usize]).collect();
    let mut data = make_rel_data(mcx, relid, form, attrs_of(&names[..]));
    let mut indkey = mcx::PgVec::new_in(mcx);
    indkey.extend_from_slice(t.indkey);
    let n = t.indkey.len() as i16;
    let src = |node: Node<'mcx>| Some(outfuncs::nodeToString(mcx, node).unwrap());
    data.rd_index = Some(types_rel::FormData_pg_index {
        indexrelid: relid,
        indrelid: table,
        indnatts: n,
        indnkeyatts: n,
        indisunique: false,
        indnullsnotdistinct: false,
        indisprimary: false,
        indisexclusion: false,
        indimmediate: true,
        indisvalid: true,
        indisready: true,
        indcheckxmin: false,
        indxmin: 0,
        indkey,
        has_indpred: t.partial,
        indexprs_src: if table == PIN_T4 {
            src(Node::mk_list(mcx, NodeList::make1(mcx, a_plus_b(mcx)).unwrap()).unwrap())
        } else {
            None
        },
        indpred_src: if t.partial { src(op(mcx, INT4_GT_OP, INT4GT_PROC, 16, var(mcx, 3), int4(mcx, 0))) } else { None },
    });
    for _ in 0..n {
        data.rd_opfamily.push(INT4_BTREE_FAM);
        data.rd_opcintype.push(23);
        data.rd_indoption.push(0);
        data.rd_indcollation.push(0);
    }
    data.rd_amcache.set(Some(types_nbtree::page::BTMetaPageData {
        btm_magic: types_nbtree::page::BTREE_MAGIC,
        btm_version: types_nbtree::page::BTREE_VERSION,
        btm_root: 3,
        btm_level: t.height,
        btm_fastroot: 3,
        btm_fastlevel: t.height,
        btm_last_cleanup_num_delpages: 0,
        btm_last_cleanup_num_heap_tuples: -1.0,
        btm_allequalimage: true,
    }));
    Some(types_rel::Relation::open(data, None))
}

/// relcache's pg_class scan for the pin tables (their index lists come from `index_shapes`).
pub(super) fn scanned_pg_class(relid: u32) -> Option<relcache_build_seams::ScannedPgClass> {
    pin_index(relid)?;
    Some(relcache_build_seams::ScannedPgClass {
        relchecks: 0,
        relnatts: 0,
        relhastriggers: false,
        relhasrules: false,
        relrewrite: 0,
        form: pin_pg_class(relid)?,
        options: None,
    })
}

pub(super) fn index_shapes(indrelid: u32) -> Option<relcache_build_seams::PgIndexListShape> {
    let t = pin_index(indrelid)?;
    Some(relcache_build_seams::PgIndexListShape {
        indexrelid: t.oid,
        indislive: true,
        indisunique: false,
        indisprimary: false,
        indimmediate: true,
        indisvalid: true,
        indisreplident: false,
        has_indpred: t.partial,
    })
}

pub(super) fn nblocks(relid: u32) -> Option<u32> {
    match pin_index(relid) {
        Some(_) => Some(HEAP_PAGES),
        None => pin_index(table_of_index(relid)?).map(|t| t.pages),
    }
}

/// pg_statistic: a (unique-ish, nearly sorted), b (50 values, two MCVs), c (1,000 values, half below 0), and the
/// expression index's own row for (a + b).
pub(super) fn statistic<'mcx>(
    mcx: Mcx<'mcx>,
    relid: u32,
    attnum: i16,
    inh: bool,
) -> Option<syscache_seams::PgStatisticBundle<'mcx>> {
    const NO_MCV: &[(i32, f32)] = &[];
    const B_MCV: &[(i32, f32)] = &[(1, 0.10), (2, 0.05)];
    if inh {
        return None;
    }
    let (distinct, mcv, hist, corr): (f32, &[(i32, f32)], Vec<i32>, f32) = match (relid, attnum) {
        (PIN_T1 | PIN_T2 | PIN_T3 | PIN_T4, 1) => (-1.0, NO_MCV, (0..=10).map(|i| i * 10_000).collect(), 0.97),
        (PIN_T1 | PIN_T2 | PIN_T3 | PIN_T4, 2) => (50.0, B_MCV, (0..=10).map(|i| i * 5).collect(), 0.3),
        (PIN_T1 | PIN_T2 | PIN_T3 | PIN_T4, 3) => (1000.0, NO_MCV, (0..=10).map(|i| i * 100 - 500).collect(), -0.2),
        (PIN_I4, 1) => (-0.5, NO_MCV, (0..=10).map(|i| i * 11_000).collect(), 0.5),
        _ => return None,
    };
    let mut slots = mcx::PgVec::new_in(mcx);
    if !mcv.is_empty() {
        let mut values = mcx::PgVec::new_in(mcx);
        let mut numbers = mcx::PgVec::new_in(mcx);
        for &(v, f) in mcv {
            values.push(Datum::from_i32(v));
            numbers.push(f);
        }
        slots.push(syscache_seams::PgStatisticSlotData::from_decoded(
            1,
            INT4EQ_OP,
            0,
            23,
            values,
            numbers,
            mcx::PgVec::new_in(mcx),
        ));
    }
    let mut values = mcx::PgVec::new_in(mcx);
    values.extend(hist.into_iter().map(Datum::from_i32));
    slots.push(syscache_seams::PgStatisticSlotData::from_decoded(
        2,
        INT4_LT_OP,
        0,
        23,
        values,
        mcx::PgVec::new_in(mcx),
        mcx::PgVec::new_in(mcx),
    ));
    let mut numbers = mcx::PgVec::new_in(mcx);
    numbers.push(corr);
    slots.push(syscache_seams::PgStatisticSlotData::from_decoded(
        3,
        INT4_LT_OP,
        0,
        23,
        mcx::PgVec::new_in(mcx),
        numbers,
        mcx::PgVec::new_in(mcx),
    ));
    Some(syscache_seams::PgStatisticBundle { stanullfrac: 0.0, stawidth: 4, stadistinct: distinct, slots })
}

fn query<'mcx>(mcx: Mcx<'mcx>, relid: u32, qual: Node<'mcx>) -> Query<'mcx> {
    let mut rte = Node::build::<types_nodes::parsenodes::RangeTblEntry>(mcx).unwrap();
    rte.rtekind = RTEKind::RTE_RELATION;
    rte.relid = relid;
    rte.relkind = b'r';
    rte.rellockmode = 1;
    rte.inh = false;
    let rtable = NodeList::make1(mcx, rte.seal()).unwrap();
    let rtr = Node::mk_range_tbl_ref(mcx, 1).unwrap();
    let jointree =
        alloc_leak_in(mcx, FromExpr { fromlist: NodeList::make1(mcx, rtr).unwrap(), quals: Some(qual) }).unwrap();
    let mut target_list = NodeList::nil();
    for (i, n) in ["a", "b", "c"].into_iter().enumerate() {
        let tle = Node::mk_target_entry(mcx, var(mcx, i as i16 + 1), i as i16 + 1, Some(n), false).unwrap();
        target_list.lappend(mcx, tle).unwrap();
    }
    Query {
        commandType: CmdType::CMD_SELECT,
        canSetTag: true,
        jointree: Some(jointree),
        rtable,
        targetList: target_list,
        stmt_location: 0,
        stmt_len: 40,
        ..Query::default()
    }
}

/// Plans `qual` over `relid` and prints one line per IndexPath among the base rel's surviving paths.
fn case_lines(case: &str, relid: u32, qual: impl for<'m> FnOnce(Mcx<'m>) -> Node<'m>) -> Vec<String> {
    let cx = cx();
    let mcx = cx.mcx();
    let mut run = crate::run::PlannerRun::new(mcx);
    crate::subquery::subquery_planner(&mut run, leak_q(mcx, query(mcx, relid, qual(mcx))), false, 0.0, None).unwrap();
    let base = crate::relnode::find_base_rel_noerr(&run.root, 1).expect("base rel 1");
    let mut ids = Vec::new();
    for &p in run.root.rel(base).pathlist.iter() {
        match run.root.path(p) {
            PathNode::IndexPath(_) => ids.push((p, "")),
            PathNode::BitmapHeapPath(bp) => {
                let mut stack: Vec<_> = bp.bitmapqual.into_iter().collect();
                while let Some(q) = stack.pop() {
                    match run.root.path(q) {
                        PathNode::IndexPath(_) => ids.push((q, "bitmap ")),
                        PathNode::BitmapAndPath(b) => stack.extend(b.bitmapquals.iter().rev().copied()),
                        PathNode::BitmapOrPath(b) => stack.extend(b.bitmapquals.iter().rev().copied()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (id, how) in ids {
        let (head, tag) = {
            let PathNode::IndexPath(ip) = run.root.path(id) else { unreachable!() };
            let p = &ip.path;
            (
                format!(
                    "{case}: {how}index {} rows {:?} startup {:?} total {:?} indextotal {:?} indexsel {:?}",
                    ip.indexinfo.expect("indexinfo set").indexoid,
                    p.rows,
                    p.startup_cost,
                    p.total_cost,
                    ip.indextotalcost,
                    ip.indexselectivity
                ),
                p.pathtype,
            )
        };
        let mut line = format!("{head} pathtype {tag}");
        for loops in [1.0, 25.0] {
            let e = crate::selfuncs::amcostestimate(&mut run, id, loops).unwrap();
            line.push_str(&format!(
                "; am({loops:?}) {:?} {:?} {:?} {:?} {:?}",
                e.index_startup_cost, e.index_total_cost, e.index_selectivity, e.index_correlation, e.index_pages
            ));
        }
        out.push(line);
    }
    assert!(!out.is_empty(), "{case}: no IndexPath survived");
    out
}

/// Captured from e59aa10685f (planner tests built with the check-test profile on aarch64).
const GOLDEN: &[&str] = &[
    "a = 42: index 16431 rows 1.0 startup 0.2925 total 8.31 indextotal 4.300000000000001 indexsel 1e-5 pathtype 341; am(1.0) 0.2925 4.300000000000001 1e-5 0.9700000286102295 1.0; am(25.0) 0.2925 4.14 1e-5 0.9700000286102295 1.0",
    "a > 42000 AND a < 44500: index 16431 rows 2499.0 startup 0.2925 total 341.0174795387236 indextotal 53.2825 indexsel 0.024989999999999846 pathtype 341; am(1.0) 0.2925 53.2825 0.024989999999999846 0.9700000286102295 7.0; am(25.0) 0.2925 46.5625 0.024989999999999846 0.9700000286102295 7.0",
    "a = ANY ({1,500,70000}): index 16431 rows 3.0 startup 0.2925 total 17.402799555969235 indextotal 12.9 indexsel 3.0000000000000004e-5 pathtype 341; am(1.0) 0.2925 12.9 3.0000000000000004e-5 0.9700000286102295 1.0; am(25.0) 0.2925 11.620000000000001 3.0000000000000004e-5 0.9700000286102295 1.0",
    "a = 5 AND c = 7 on (a, b, c): index 16433 rows 1.0 startup 0.4175 total 217.81249999999997 indextotal 213.80249999999998 indexsel 1e-8 pathtype 342; am(1.0) 0.4175 213.80249999999998 1e-8 0.7275000214576721 1.0; am(25.0) 0.4175 83.5625 1e-8 0.7275000214576721 1.0",
    "a < 44500 AND b = 2 on (a, b, c): bitmap index 16433 rows 2225.0 startup 0.4175 total 3052.39303842809 indextotal 1133.4075 indexsel 0.022249500331543385 pathtype 342; am(1.0) 0.4175 1133.4075 0.022249500331543385 0.7275000214576721 172.0; am(25.0) 0.4175 507.1675 0.022249500331543385 0.7275000214576721 172.0",
    "b < 20 AND c > 0 on (b) WHERE c > 0: bitmap index 16435 rows 23615.0 startup 0.29 total 4339.152473106384 indextotal 441.40250000000003 indexsel 0.23614583402716866 pathtype 341; am(1.0) 0.29 441.40250000000003 0.23614583402716866 0.30000001192092896 66.0; am(25.0) 0.29 199.64249999999998 0.23614583402716866 0.30000001192092896 66.0",
    "(a + b) = 10 on ((a + b)): index 16437 rows 2.0 startup 0.2925 total 11.3275 indextotal 4.3075 indexsel 2e-5 pathtype 341; am(1.0) 0.2925 4.3075 2e-5 0.5 1.0; am(25.0) 0.2925 4.1475 2e-5 0.5 1.0",
    "(a + b) > 50000 AND (a + b) < 52000 on ((a + b)): bitmap index 16437 rows 1816.0 startup 0.2925 total 2922.1124999999997 indextotal 42.4525 indexsel 0.01816181818181839 pathtype 341; am(1.0) 0.2925 42.4525 0.01816181818181839 0.5 6.0; am(25.0) 0.2925 37.332499999999996 0.01816181818181839 0.5 6.0",
];

#[test]
fn btcostestimate_outputs_are_pinned() {
    let mut got = Vec::new();
    got.extend(case_lines("a = 42", PIN_T1, |m| op(m, INT4EQ_OP, INT4EQ_PROC, 16, var(m, 1), int4(m, 42))));
    got.extend(case_lines("a > 42000 AND a < 44500", PIN_T1, |m| {
        and(
            m,
            op(m, INT4_GT_OP, INT4GT_PROC, 16, var(m, 1), int4(m, 42_000)),
            op(m, INT4_LT_OP, INT4LT_PROC, 16, var(m, 1), int4(m, 44_500)),
        )
    }));
    got.extend(case_lines("a = ANY ({1,500,70000})", PIN_T1, |m| {
        Node::mk(
            m,
            ScalarArrayOpExpr {
                opno: INT4EQ_OP,
                opfuncid: INT4EQ_PROC,
                hashfuncid: 0,
                negfuncid: 0,
                useOr: true,
                inputcollid: 0,
                args: NodeList::make2(m, var(m, 1), int4_array(m, &[1, 500, 70_000])).unwrap(),
                location: -1,
            },
        )
        .unwrap()
    }));
    got.extend(case_lines("a = 5 AND c = 7 on (a, b, c)", PIN_T2, |m| {
        and(
            m,
            op(m, INT4EQ_OP, INT4EQ_PROC, 16, var(m, 1), int4(m, 5)),
            op(m, INT4EQ_OP, INT4EQ_PROC, 16, var(m, 3), int4(m, 7)),
        )
    }));
    got.extend(case_lines("a < 44500 AND b = 2 on (a, b, c)", PIN_T2, |m| {
        and(
            m,
            op(m, INT4_LT_OP, INT4LT_PROC, 16, var(m, 1), int4(m, 44_500)),
            op(m, INT4EQ_OP, INT4EQ_PROC, 16, var(m, 2), int4(m, 2)),
        )
    }));
    got.extend(case_lines("b < 20 AND c > 0 on (b) WHERE c > 0", PIN_T3, |m| {
        and(
            m,
            op(m, INT4_LT_OP, INT4LT_PROC, 16, var(m, 2), int4(m, 20)),
            op(m, INT4_GT_OP, INT4GT_PROC, 16, var(m, 3), int4(m, 0)),
        )
    }));
    got.extend(case_lines("(a + b) = 10 on ((a + b))", PIN_T4, |m| {
        op(m, INT4EQ_OP, INT4EQ_PROC, 16, a_plus_b(m), int4(m, 10))
    }));
    got.extend(case_lines("(a + b) > 50000 AND (a + b) < 52000 on ((a + b))", PIN_T4, |m| {
        and(
            m,
            op(m, INT4_GT_OP, INT4GT_PROC, 16, a_plus_b(m), int4(m, 50_000)),
            op(m, INT4_LT_OP, INT4LT_PROC, 16, a_plus_b(m), int4(m, 52_000)),
        )
    }));
    if got != GOLDEN {
        panic!(
            "btcostestimate outputs moved off the pin (captured from e59aa10685f); actual:\n{}",
            got.iter().map(|l| format!("    {l:?},")).collect::<Vec<_>>().join("\n")
        );
    }
}
