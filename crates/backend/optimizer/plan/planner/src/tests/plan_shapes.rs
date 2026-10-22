use super::*;
use types_nodes::parsenodes::SortGroupClause;
use types_nodes::primnodes::Aggref;

const COUNT_STAR: u32 = 2803;
pub(super) const SUM_INT4: u32 = 2108;
pub(super) const MAX_INT4: u32 = 2116;
pub(super) const INT4PL_OP: u32 = 551;
pub(super) const INT4PL_PROC: u32 = 177;
pub(super) const INT8OID: u32 = 20;
pub(super) const INT4OID: u32 = 23;

fn ensure_work_mem() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !guc_tables::vars::work_mem.installed() {
            init_small::init_seams();
        }
    });
}

pub(super) fn setup() -> MemoryContext {
    install_fixtures();
    ensure_work_mem();
    cx()
}

struct ColumnarPosture;
impl ColumnarPosture {
    fn hold() -> Self {
        crate::grouping::HASHED_DISTINCT_ON_HEAP_FOR_TESTS.store(true, std::sync::atomic::Ordering::Relaxed);
        ColumnarPosture
    }
}
impl Drop for ColumnarPosture {
    fn drop(&mut self) {
        crate::grouping::HASHED_DISTINCT_ON_HEAP_FOR_TESTS.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn var(mcx: Mcx<'_>, attno: i16) -> Node<'_> {
    Node::mk_var(mcx, 1, attno, INT4OID, -1, 0, 0).unwrap()
}

pub(super) fn tle<'m>(mcx: Mcx<'m>, expr: Node<'m>, resno: i16, name: &'m str, sortgroupref: u32) -> Node<'m> {
    let t = Node::mk_target_entry(mcx, expr, resno, Some(name), false).unwrap();
    if sortgroupref != 0 {
        // SAFETY: freshly built entry; no other reference is live.
        unsafe { t.with_mut::<types_nodes::primnodes::TargetEntry, _>(|t| t.ressortgroupref = sortgroupref) }
            .unwrap();
    }
    t
}

fn agg_arg<'m>(mcx: Mcx<'m>, expr: Node<'m>, sortgroupref: u32) -> (types_nodes::list::OidList<'m>, NodeList<'m>) {
    let mut types = types_nodes::list::OidList::nil();
    types.lappend(mcx, INT4OID).unwrap();
    let arg = tle(mcx, expr, 1, "arg", sortgroupref);
    (types, NodeList::make1(mcx, arg).unwrap())
}

pub(super) fn count_star(mcx: Mcx<'_>) -> Node<'_> {
    Node::mk(mcx, Aggref { aggfnoid: COUNT_STAR, aggtype: INT8OID, aggstar: true, ..Aggref::default() }).unwrap()
}

pub(super) fn sum_of<'m>(mcx: Mcx<'m>, expr: Node<'m>, distinct: bool) -> Node<'m> {
    let (aggargtypes, args) = agg_arg(mcx, expr, u32::from(distinct));
    let aggdistinct = if distinct {
        NodeList::make1(mcx, sgc(mcx, 1, INT4EQ_OP, INT4_LT_OP, false)).unwrap()
    } else {
        NodeList::nil()
    };
    Node::mk(mcx, Aggref { aggfnoid: SUM_INT4, aggtype: INT8OID, aggargtypes, args, aggdistinct, ..Aggref::default() })
        .unwrap()
}

/// `sum(x ORDER BY x)`: an ORDER BY aggregate (never hashed).
pub(super) fn sum_ordered<'m>(mcx: Mcx<'m>, expr: Node<'m>) -> Node<'m> {
    let (aggargtypes, args) = agg_arg(mcx, expr, 1);
    let aggorder = NodeList::make1(mcx, sgc(mcx, 1, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
    Node::mk(mcx, Aggref { aggfnoid: SUM_INT4, aggtype: INT8OID, aggargtypes, args, aggorder, ..Aggref::default() })
        .unwrap()
}

pub(super) fn max_of<'m>(mcx: Mcx<'m>, expr: Node<'m>) -> Node<'m> {
    let (aggargtypes, args) = agg_arg(mcx, expr, 0);
    Node::mk(mcx, Aggref { aggfnoid: MAX_INT4, aggtype: INT4OID, aggargtypes, args, ..Aggref::default() }).unwrap()
}

pub(super) fn plus_one<'m>(mcx: Mcx<'m>, expr: Node<'m>) -> Node<'m> {
    let one = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(1), false, true).unwrap();
    Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno: INT4PL_OP,
            opfuncid: INT4PL_PROC,
            opresulttype: INT4OID,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, expr, one).unwrap(),
            location: -1,
        },
    )
    .unwrap()
}

pub(super) fn plus_pk<'m>(mcx: Mcx<'m>, expr: Node<'m>) -> Node<'m> {
    Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno: INT4PL_OP,
            opfuncid: INT4PL_PROC,
            opresulttype: INT4OID,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, expr, var(mcx, 1)).unwrap(),
            location: -1,
        },
    )
    .unwrap()
}

pub(super) fn sgc(mcx: Mcx<'_>, sortgroupref: u32, eqop: u32, sortop: u32, desc: bool) -> Node<'_> {
    Node::mk(
        mcx,
        SortGroupClause {
            tleSortGroupRef: sortgroupref,
            eqop,
            sortop,
            reverse_sort: desc,
            nulls_first: desc,
            hashable: true,
        },
    )
    .unwrap()
}

pub(super) fn limit<'m>(parse: &mut Query<'m>, mcx: Mcx<'m>, n: i64) {
    parse.limitCount = Some(Node::mk_const(mcx, INT8OID, -1, 0, 8, Datum::from_i64(n), false, true).unwrap());
    parse.limitOption = types_nodes::nodes_enums::LimitOption::LIMIT_OPTION_COUNT;
}

pub(super) fn query<'m>(
    mcx: Mcx<'m>,
    qual: Option<Node<'m>>,
    tlist: &[(Node<'m>, &'m str, u32)],
    group_refs: &[u32],
    order: Option<(u32, u32, u32, bool)>,
) -> Query<'m> {
    let mut parse = table_query(mcx, qual);
    let mut tl = NodeList::nil();
    for (i, (expr, name, r)) in tlist.iter().enumerate() {
        tl.lappend(mcx, tle(mcx, *expr, (i + 1) as i16, name, *r)).unwrap();
    }
    parse.targetList = tl;
    parse.hasAggs = tlist.iter().any(|(e, _, _)| e.node_tag() == NodeTag::T_Aggref);
    let mut gc = NodeList::nil();
    for r in group_refs {
        gc.lappend(mcx, sgc(mcx, *r, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
    }
    parse.groupClause = gc;
    if let Some((r, eqop, sortop, desc)) = order {
        parse.sortClause = NodeList::make1(mcx, sgc(mcx, r, eqop, sortop, desc)).unwrap();
    }
    parse
}

pub(super) fn plan_of<'m>(mcx: Mcx<'m>, parse: Query<'m>, sql: &str) -> Node<'m> {
    let stmt = planner(mcx, leak_q(mcx, parse), sql, CURSOR_OPT_PARALLEL_OK, ParamListHandle::NULL).unwrap();
    stmt.planTree.unwrap()
}

pub(super) fn chain(plan: &Node<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Some(*plan);
    while let Some(p) = cur {
        let name = match p.node_tag() {
            NodeTag::T_Limit => "Limit".to_string(),
            NodeTag::T_Sort => "Sort".to_string(),
            NodeTag::T_SeqScan => "Scan".to_string(),
            NodeTag::T_IndexScan => "IndexScan".to_string(),
            NodeTag::T_Agg => {
                let a = p.as_agg().unwrap();
                let s = match a.aggstrategy {
                    types_pathnodes::AGG_HASHED => "HashAgg",
                    types_pathnodes::AGG_SORTED => "GroupAgg",
                    _ => "Agg",
                };
                if a.numCols > 0 { format!("{s}{{{}}}", a.numCols) } else { s.to_string() }
            }
            other => format!("{other:?}"),
        };
        out.push(name);
        cur = p.as_plan().unwrap().lefttree;
    }
    out
}

pub(super) fn assert_chain(plan: &Node<'_>, wanted: &[&str]) {
    assert_eq!(chain(plan), wanted, "plan chain");
}

#[test]
fn plain_aggregate_plans_agg_over_scan() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(count_star(mcx), "count", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT count(*) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    assert_eq!(plan.as_agg().unwrap().aggstrategy, types_pathnodes::AGG_PLAIN);
}

#[test]
fn distinct_aggregate_without_group_by_hashes_the_distinct_set() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(sum_of(mcx, var(mcx, 2), true), "sum", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT val) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    let agg = plan.as_agg().unwrap();
    assert_eq!(agg.aggstrategy, types_pathnodes::AGG_PLAIN);
    assert!(agg.distinctHashed, "Distinct: hashed");
    let aggref = agg.plan.targetlist.nth(0).as_target_entry().unwrap().expr.as_aggref().unwrap();
    assert!(!aggref.aggpresorted, "the hashed plan's input is not presorted");
}

// the identity set keeps the first-arriving member of each equal run, PostgreSQL's sort the first in pg_qsort's
// order, so a DISTINCT aggregate whose members can print apart keeps the sorted path (an unconstrained numeric's display
// scale, a float's zero sign, interval); count(DISTINCT) and an argument whose members print alike keep the hashed form.
#[test]
fn d2865_hashed_distinct_only_where_members_print_alike() {
    let cx = setup();
    let mcx = cx.mcx();
    let call = |fnoid: u32, typid: u32, typmod: i32| {
        let v = Node::mk_var(mcx, 1, 2, typid, typmod, 0, 0).unwrap();
        let args = NodeList::make1(mcx, tle(mcx, v, 1, "arg", 1)).unwrap();
        let aggdistinct = NodeList::make1(mcx, sgc(mcx, 1, 0, 0, false)).unwrap();
        Node::mk(mcx, Aggref { aggfnoid: fnoid, aggdistinct, args, ..Aggref::default() }).unwrap()
    };
    let alike = |n: Node<'_>| crate::grouping::hashed_distinct_members_alike(n.as_aggref().unwrap()).unwrap();
    assert!(alike(call(SUM_INT4, INT4OID, -1)), "sum(DISTINCT int4)");
    assert!(alike(call(2147, NUMERICOID, -1)), "count(DISTINCT numeric) counts the runs");
    assert!(!alike(call(2114, NUMERICOID, -1)), "sum(DISTINCT numeric): 1.0 / 1.00");
    assert!(alike(call(2114, NUMERICOID, (12 << 16 | 2) + 4)), "sum(DISTINCT numeric(12,2))");
    assert!(!alike(call(2111, 701, -1)), "sum(DISTINCT float8): -0 / 0");
    assert!(alike(call(2105, 701, -1)), "avg(DISTINCT float8): float8_accum's sums from +0 hide the zero sign");
    assert!(alike(call(2644, 700, -1)), "var_samp(DISTINCT float4): float4_accum's");
    assert!(!alike(call(2110, 700, -1)), "sum(DISTINCT float4): -0 / 0");
    assert!(!alike(call(2335, 701, -1)), "array_agg(DISTINCT float8): -0 / 0");
    assert!(!alike(call(2335, 1186, -1)), "array_agg(DISTINCT interval): 1 day / 24:00:00");
    assert!(alike(call(2335, 1007, -1)), "array_agg(DISTINCT int4[])");
}

#[test]
fn d2865_count_distinct_keeps_the_hashed_form() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let (aggargtypes, args) = agg_arg(mcx, var(mcx, 2), 1);
    let aggdistinct = NodeList::make1(mcx, sgc(mcx, 1, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
    let count = Node::mk(mcx, Aggref { aggfnoid: 2147, aggtype: INT8OID, aggargtypes, args, aggdistinct, ..Aggref::default() }).unwrap();
    let parse = query(mcx, None, &[(count, "count", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT count(DISTINCT val) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    assert!(plan.as_agg().unwrap().distinctHashed, "Distinct: hashed");
}

// A heap-only query keeps PostgreSQL's plan: the TPC-C stock-level count(DISTINCT) must not move.
#[test]
fn distinct_aggregate_on_heap_keeps_the_sorted_path() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(sum_of(mcx, var(mcx, 2), true), "sum", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT val) FROM t");
    assert_chain(&plan, &["Agg", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed);
}

#[test]
fn order_by_aggregate_keeps_the_sorted_path() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(sum_ordered(mcx, var(mcx, 2)), "sum", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(val ORDER BY val) FROM t");
    assert_chain(&plan, &["Agg", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed);
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (sum_ordered(mcx, var(mcx, 1)), "sum", 0), (max_of(mcx, var(mcx, 1)), "m", 2)],
        &[1],
        Some((2, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, sum(pk ORDER BY pk), max(pk) m FROM t GROUP BY val ORDER BY m LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "GroupAgg{1}", "Sort", "Scan"]);
}

#[test]
fn two_distinct_aggregates_over_different_columns_hash() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(
        mcx,
        None,
        &[(sum_of(mcx, var(mcx, 1), true), "s1", 0), (sum_of(mcx, var(mcx, 2), true), "s2", 0)],
        &[],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT pk), sum(DISTINCT val) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    assert!(plan.as_agg().unwrap().distinctHashed);
}

#[test]
fn distinct_riders_hash_only_when_order_free() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let first_val = |x| {
        let (aggargtypes, args) = agg_arg(mcx, x, 0);
        let a = Aggref { aggfnoid: FIRST_VAL_ANYELEMENT, aggtype: INT4OID, aggargtypes, args, ..Aggref::default() };
        Node::mk(mcx, a).unwrap()
    };
    let parse = query(mcx, None, &[(sum_of(mcx, var(mcx, 2), true), "s", 0), (count_star(mcx), "c", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT val), count(*) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    assert!(plan.as_agg().unwrap().distinctHashed, "a count rides the hashed set");
    let parse = query(mcx, None, &[(sum_of(mcx, var(mcx, 2), true), "s", 0), (first_val(var(mcx, 1)), "f", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT val), first_val(pk) FROM t");
    assert_chain(&plan, &["Agg", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed, "an order-reading rider keeps the sorted path");
    let parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "s", 0), (first_val(var(mcx, 1)), "f", 0)],
        &[1],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT val, sum(DISTINCT pk), first_val(pk) FROM t GROUP BY val");
    assert_chain(&plan, &["GroupAgg{1}", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed, "grouped: the sorted path too");
}

// the hashed set evaluates every aggregate's arguments and FILTER per input row in the scan's order, PostgreSQL
// in the presorted order: a VOLATILE call there keeps Agg over Sort, beside an order-free rider or with none.
#[test]
fn distinct_riders_with_a_volatile_call_keep_the_sorted_path() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let nextval = || Node::mk(mcx, types_nodes::primnodes::NextValueExpr { seqid: 16384, typeId: INT4OID }).unwrap();
    let filtered = |agg| with_filter(agg, int_op(mcx, INT4EQ_OP, INT4EQ_PROC, 16, var(mcx, 1), nextval()));
    let cases = [
        ("SELECT sum(DISTINCT val), sum(nextval('s') + 1) FROM t", sum_of(mcx, plus_one(mcx, nextval()), false)),
        ("SELECT sum(DISTINCT val), count(*) FILTER (WHERE pk = nextval('s')) FROM t", filtered(count_star(mcx))),
        ("SELECT sum(DISTINCT val), sum(DISTINCT nextval('s') + 1) FROM t", sum_of(mcx, plus_one(mcx, nextval()), true)),
    ];
    for (sql, rider) in cases {
        let parse = query(mcx, None, &[(sum_of(mcx, var(mcx, 2), true), "s", 0), (rider, "r", 0)], &[], None);
        let plan = plan_of(mcx, parse, sql);
        assert_chain(&plan, &["Agg", "Sort", "Scan"]);
        assert!(!plan.as_agg().unwrap().distinctHashed, "{sql}");
    }
    let parse = query(mcx, None, &[(filtered(sum_of(mcx, var(mcx, 2), true)), "s", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(DISTINCT val) FILTER (WHERE pk = nextval('s')) FROM t");
    assert_chain(&plan, &["Agg", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed, "a volatile FILTER on the DISTINCT aggregate itself");
    let tl = [(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "s", 0), (filtered(count_star(mcx)), "c", 0)];
    let sql = "SELECT val, sum(DISTINCT pk), count(*) FILTER (WHERE pk = nextval('s')) FROM t GROUP BY val";
    let plan = plan_of(mcx, query(mcx, None, &tl, &[1], None), sql);
    assert_chain(&plan, &["GroupAgg{1}", "Sort", "Scan"]);
    assert!(!plan.as_agg().unwrap().distinctHashed, "grouped");
    let tl = [(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "s", 0), (nextval(), "n", 0)];
    let plan = plan_of(mcx, query(mcx, None, &tl, &[1], None), "SELECT val, sum(DISTINCT pk), nextval('s') FROM t GROUP BY val");
    assert_chain(&plan, &["GroupAgg{1}", "Sort", "Scan"]);
    let mut parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "s", 0)], &[1], None);
    parse.havingQual = Some(int_op(mcx, INT4EQ_OP, INT4EQ_PROC, 16, var(mcx, 2), nextval()));
    let plan = plan_of(mcx, parse, "SELECT val, sum(DISTINCT pk) FROM t GROUP BY val HAVING val = nextval('s')");
    assert_chain(&plan, &["GroupAgg{1}", "Sort", "Scan"]);
    let parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "s", 0)], &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(DISTINCT pk) FROM t GROUP BY val");
    assert_chain(&plan, &["HashAgg{1}", "Scan"]);
    assert!(plan.as_agg().unwrap().distinctHashed, "the control hashes");
}

fn with_filter<'m>(agg: Node<'m>, filter: Node<'m>) -> Node<'m> {
    // SAFETY: freshly built Aggref; no other reference is live.
    unsafe { agg.with_mut::<Aggref, _>(|a| a.aggfilter = Some(filter)) }.unwrap();
    agg
}

pub(super) fn dat_entries(text: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let text: String = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
    let b = text.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        if b[i] != b'{' {
            i += 1;
            continue;
        }
        let mut e = std::collections::BTreeMap::new();
        i += 1;
        loop {
            while !(b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'}') {
                i += 1;
            }
            if b[i] == b'}' {
                i += 1;
                break;
            }
            let k0 = i;
            while b[i].is_ascii_alphanumeric() || b[i] == b'_' {
                i += 1;
            }
            let key = text[k0..i].to_string();
            while b[i] != b'\'' {
                i += 1;
            }
            let mut v = Vec::new();
            i += 1;
            while b[i] != b'\'' {
                if b[i] == b'\\' {
                    i += 1;
                }
                v.push(b[i]);
                i += 1;
            }
            i += 1;
            e.insert(key, String::from_utf8_lossy(&v).into_owned());
        }
        out.push(e);
    }
    out
}

#[test]
fn rider_order_matches_pg_proc_and_lowerv4() {
    use crate::grouping::{rider_order, RiderOrder, RiderOrder::*};
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let read = |p: &str| std::fs::read_to_string(crates.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    #[rustfmt::skip]
    let pinned: &[(u32, &str, &str, RiderOrder)] = &[
        (2147, "count", "any", Free), (2803, "count", "", Free), (2818, "regr_count", "float8 float8", Free),
        (2107, "sum", "int8", Free), (2108, "sum", "int4", Free), (2109, "sum", "int2", Free), (2114, "sum", "numeric", Free),
        (2100, "avg", "int8", Free), (2101, "avg", "int4", Free), (2102, "avg", "int2", Free), (2103, "avg", "numeric", Free),
        (2517, "bool_and", "bool", Free), (2518, "bool_or", "bool", Free), (2519, "every", "bool", Free),
        (2236, "bit_and", "int2", Free), (2237, "bit_or", "int2", Free), (2238, "bit_and", "int4", Free),
        (2239, "bit_or", "int4", Free), (2240, "bit_and", "int8", Free), (2241, "bit_or", "int8", Free),
        (6164, "bit_xor", "int2", Free), (6165, "bit_xor", "int4", Free), (6166, "bit_xor", "int8", Free),
        (2115, "max", "int8", Free), (2116, "max", "int4", Free), (2117, "max", "int2", Free), (2118, "max", "oid", Free),
        (2122, "max", "date", Free), (2123, "max", "time", Free), (2125, "max", "money", Free),
        (2126, "max", "timestamp", Free), (2127, "max", "timestamptz", Free),
        (2131, "min", "int8", Free), (2132, "min", "int4", Free), (2133, "min", "int2", Free), (2134, "min", "oid", Free),
        (2138, "min", "date", Free), (2139, "min", "time", Free), (2141, "min", "money", Free),
        (2142, "min", "timestamp", Free), (2143, "min", "timestamptz", Free),
        (2797, "max", "tid", Free), (2798, "min", "tid", Free), (4189, "max", "pg_lsn", Free), (4190, "min", "pg_lsn", Free),
        (5099, "max", "xid8", Free), (5100, "min", "xid8", Free), (6395, "max", "bytea", Free), (6396, "min", "bytea", Free),
        (2129, "max", "text", DeterministicCollation), (2145, "min", "text", DeterministicCollation),
        (2130, "max", "numeric", Typmod), (2146, "min", "numeric", Typmod),
    ];
    let aggs: std::collections::BTreeMap<u32, (String, String)> =
        dat_entries(&read("postgres-18.6-reference/src/include/catalog/pg_proc.dat"))
            .into_iter()
            .filter(|e| e.get("prokind").is_some_and(|k| k == "a"))
            .map(|e| (e["oid"].parse().unwrap(), (e["proname"].clone(), e.get("proargtypes").cloned().unwrap_or_default())))
            .collect();
    for &(oid, name, args, class) in pinned {
        assert_eq!(aggs.get(&oid), Some(&(name.to_string(), args.to_string())), "pg_proc.dat oid {oid}");
        assert_eq!(rider_order(oid), class, "{name}({args}) oid {oid}");
    }
    for oid in 0..20_000 {
        assert!(rider_order(oid) == Reads || pinned.iter().any(|p| p.0 == oid), "oid {oid}: a rider class not pinned");
    }
    let fold = read("gravity/lowerv4/src/effects/fold.rs").split_whitespace().collect::<Vec<_>>().join(" ");
    for pin in [
        "let exact = |c| matches!(c, CountStar | CountAny | CountPair | SumI64 | SumI128 | SumNum | AvgI128 | AvgNum | BoolAnd | BoolOr | BitI64(_)) || matches!(c, Min(Ext::Int | Ext::U64 | Ext::Tid) | Max(Ext::Int | Ext::U64 | Ext::Tid));",
        "Min(Ext::Text | Ext::Bytes) | Max(Ext::Text | Ext::Bytes) => d.coll.bytes_eq && args_one_image(d.fnoid),",
        "Min(Ext::Num | Ext::Opaque) | Max(Ext::Num | Ext::Opaque) => matches!(d.reg, Register::MinNum | Register::MaxNum),",
        "!kept && g.aggs.iter().all(|d| exact(d.class) || home(d))",
        "order_free(g) && g.aggs.iter().all(member)",
    ] {
        assert!(
            fold.contains(pin),
            "lowerv4 effects/fold.rs order_free changed: re-derive rider_order and this pin: {pin}"
        );
    }
    let lowerv4 = |fnoid: u32, class: &str| {
        let ext = |e: &str| class == format!("Min(Ext::{e})") || class == format!("Max(Ext::{e})");
        let head = class.split('(').next().unwrap();
        let counts = ["CountStar", "CountAny", "CountPair", "SumI64", "SumI128", "SumNum", "AvgI128", "AvgNum"];
        if counts.contains(&head) || ["BoolAnd", "BoolOr", "BitI64"].contains(&head) || ["Int", "U64", "Tid", "Bytes"].into_iter().any(ext) {
            Free
        } else if ext("Text") && !matches!(fnoid, 2244 | 2245) {
            DeterministicCollation
        } else if ext("Num") {
            Typmod
        } else {
            Reads
        }
    };
    let mut classed = Vec::new();
    for line in read("gravity/decompose/src/ops/agg/trans/table.rs").lines() {
        let Some(row) = line.trim().strip_prefix("AggRow { fnoid: ") else { continue };
        let fnoid: u32 = row[..row.find(',').unwrap()].parse().unwrap();
        let class = row[row.find("class: Class::").unwrap() + 14..].trim_end_matches(" },");
        assert_eq!(rider_order(fnoid), lowerv4(fnoid, class), "fnoid {fnoid} ({class}): planner vs lowerv4 order_free");
        classed.push(fnoid);
    }
    assert!(classed.len() > 150 && pinned.iter().all(|p| classed.contains(&p.0)), "aggtab.rs rows {}", classed.len());
}

#[test]
fn distinct_aggregate_under_group_by_hashes() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (sum_of(mcx, var(mcx, 1), true), "sum", 0), (max_of(mcx, var(mcx, 1)), "m", 2)],
        &[1],
        Some((2, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, sum(DISTINCT pk), max(pk) m FROM t GROUP BY val ORDER BY m LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "HashAgg{1}", "Scan"]);
    let agg = plan.as_plan().unwrap().lefttree.unwrap().as_plan().unwrap().lefttree.unwrap();
    assert!(agg.as_agg().unwrap().distinctHashed, "Distinct: hashed");
}

#[test]
fn group_by_order_by_aggregate_limit_plans_limit_sort_hashagg() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (max_of(mcx, var(mcx, 1)), "m", 2)],
        &[1],
        Some((2, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, max(pk) m FROM t GROUP BY val ORDER BY m LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "HashAgg{1}", "Scan"]);
}

#[test]
fn group_by_limit_without_order_by_plans_limit_hashagg() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (count_star(mcx), "count", 0)], &[1], None);
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, count(*) FROM t GROUP BY val LIMIT 10");
    assert_chain(&plan, &["Limit", "HashAgg{1}", "Scan"]);
}

#[test]
fn equality_qual_plans_scan_with_one_filter() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, Some(eq_qual(mcx, 2, 5)), &[(var(mcx, 2), "val", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT val FROM t WHERE val = 5");
    assert_chain(&plan, &["Scan"]);
    assert_eq!(plan.as_seq_scan().unwrap().scan.plan.qual.len(), 1);
}

#[test]
fn order_by_limit_plans_limit_sort_scan() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 1), "pk", 0), (var(mcx, 2), "val", 1)],
        &[],
        Some((1, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT pk, val FROM t ORDER BY val LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "Scan"]);
}

pub(super) const INT4MI_OP: u32 = 555;
pub(super) const INT4MI_PROC: u32 = 181;
pub(super) const NUMERICOID: u32 = 1700;

pub(super) fn int_op<'m>(mcx: Mcx<'m>, opno: u32, opfuncid: u32, restype: u32, l: Node<'m>, r: Node<'m>) -> Node<'m> {
    Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
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

#[test]
fn dependent_grouping_key_is_trimmed() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (max_of(mcx, var(mcx, 1)), "m", 3)],
        &[1, 2],
        Some((3, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, val + 1, max(pk) m FROM t GROUP BY val, val + 1 ORDER BY m LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "HashAgg{1}", "Scan"]);
    let agg = plan.as_plan().unwrap().lefttree.unwrap().as_plan().unwrap().lefttree.unwrap();
    let tl = &agg.as_plan().unwrap().targetlist;
    assert!(
        tl.iter().any(|t| t.as_target_entry().unwrap().expr.as_op_expr().is_some_and(|o| o.opno == INT4PL_OP)),
        "val + 1 is recomputed in the Agg's target list"
    );
}

#[test]
fn dependent_key_function_of_a_non_key_is_kept() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 1)), "pk1", 2), (max_of(mcx, var(mcx, 1)), "m", 3)],
        &[1, 2],
        Some((3, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, pk + 1, max(pk) m FROM t GROUP BY val, pk + 1 ORDER BY m LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "HashAgg{2}", "Scan"]);
}

fn agg_keys(plan: &Node<'_>) -> Option<String> {
    chain(plan).into_iter().find(|s| s.contains("Agg"))
}

// `val + 1` overflows; PostgreSQL computes it on every input row, the trim only for the groups the Agg
// projects. Without a first ORDER BY key over an aggregate (a full sort above the Agg), a LIMIT stops before every
// group is projected, so the key stays.
#[test]
fn raising_dependent_key_without_an_aggregate_order_is_kept() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (max_of(mcx, var(mcx, 1)), "m", 0)],
        &[1, 2],
        Some((1, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, val + 1, max(pk) m FROM t GROUP BY val, val + 1 ORDER BY val LIMIT 10");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{2}")), "{:?}", chain(&plan));
    let parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (count_star(mcx), "count", 0)],
        &[1, 2],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT val, val + 1, count(*) FROM t GROUP BY val, val + 1");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{2}")), "no ORDER BY: {:?}", chain(&plan));
}

// a HAVING rejects groups the Agg never projects, so their `val + 1` never raises: the key stays.
#[test]
fn raising_dependent_key_under_having_is_kept() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (max_of(mcx, var(mcx, 1)), "m", 3)],
        &[1, 2],
        Some((3, INT4EQ_OP, INT4_LT_OP, false)),
    );
    let hundred = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(100), false, true).unwrap();
    let lt = types_nodes::primnodes::OpExpr {
        opno: INT4_LT_OP,
        opfuncid: 0,
        opresulttype: 16,
        opretset: false,
        opcollid: 0,
        inputcollid: 0,
        args: NodeList::make2(mcx, max_of(mcx, var(mcx, 1)), hundred).unwrap(),
        location: -1,
    };
    parse.havingQual = Some(Node::mk(mcx, lt).unwrap());
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, val + 1, max(pk) m FROM t GROUP BY val, val + 1 HAVING max(pk) < 100 ORDER BY m LIMIT 10");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{2}")), "{:?}", chain(&plan));
}

#[test]
fn negation_and_key_expression_dependents_are_trimmed() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let neg = Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno: 558,
            opfuncid: 212,
            opresulttype: INT4OID,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make1(mcx, var(mcx, 2)).unwrap(),
            location: -1,
        },
    )
    .unwrap();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (neg, "neg", 2), (max_of(mcx, var(mcx, 1)), "m", 3)],
        &[1, 2],
        Some((3, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, -val, max(pk) m FROM t GROUP BY val, -val ORDER BY m LIMIT 10");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{1}")), "-val: {:?}", chain(&plan));
    let zero = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(0), false, true).unwrap();
    let key = || {
        Node::mk(
            mcx,
            types_nodes::primnodes::OpExpr {
                opno: INT4PL_OP,
                opfuncid: INT4PL_PROC,
                opresulttype: INT4OID,
                opretset: false,
                opcollid: 0,
                inputcollid: 0,
                args: NodeList::make2(mcx, var(mcx, 2), zero).unwrap(),
                location: -1,
            },
        )
        .unwrap()
    };
    let one = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(1), false, true).unwrap();
    let dep = Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno: INT4MI_OP,
            opfuncid: INT4MI_PROC,
            opresulttype: INT4OID,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, key(), one).unwrap(),
            location: -1,
        },
    )
    .unwrap();
    let mut parse = query(
        mcx,
        None,
        &[(key(), "k0", 1), (dep, "k1", 2), (max_of(mcx, var(mcx, 1)), "m", 3)],
        &[1, 2],
        Some((3, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val + 0, (val + 0) - 1, max(pk) m FROM t GROUP BY 1, 2 ORDER BY m LIMIT 10");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{1}")), "(val + 0) - 1: {:?}", chain(&plan));
}

#[test]
fn widening_cast_dependent_key_is_trimmed_without_an_order() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let int8_of = Node::mk(
        mcx,
        types_nodes::primnodes::FuncExpr { funcid: 481, funcresulttype: INT8OID, args: NodeList::make1(mcx, var(mcx, 2)).unwrap(), ..Default::default() },
    )
    .unwrap();
    let parse =
        query(mcx, None, &[(var(mcx, 2), "val", 1), (int8_of, "val8", 2), (count_star(mcx), "count", 0)], &[1, 2], None);
    let plan = plan_of(mcx, parse, "SELECT val, int8(val), count(*) FROM t GROUP BY val, int8(val)");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{1}")), "{:?}", chain(&plan));
}

// a volatile key is no key. PostgreSQL computes each target's copy of `val + int4(random())` on every row, so
// `int8(val + int4(random()))` is no function of the first copy's value: both keys stay.
#[test]
fn dependent_of_a_volatile_key_is_kept() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let key = || {
        let random = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 1598, funcresulttype: 701, ..Default::default() }).unwrap();
        let int4_of =
            Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 319, funcresulttype: INT4OID, args: NodeList::make1(mcx, random).unwrap(), ..Default::default() })
                .unwrap();
        Node::mk(
            mcx,
            types_nodes::primnodes::OpExpr {
                opno: INT4PL_OP,
                opfuncid: INT4PL_PROC,
                opresulttype: INT4OID,
                opretset: false,
                opcollid: 0,
                inputcollid: 0,
                args: NodeList::make2(mcx, var(mcx, 2), int4_of).unwrap(),
                location: -1,
            },
        )
        .unwrap()
    };
    let int8_of = Node::mk(
        mcx,
        types_nodes::primnodes::FuncExpr { funcid: 481, funcresulttype: INT8OID, args: NodeList::make1(mcx, key()).unwrap(), ..Default::default() },
    )
    .unwrap();
    let parse = query(mcx, None, &[(key(), "r", 1), (int8_of, "r8", 2), (count_star(mcx), "count", 0)], &[1, 2], None);
    let plan = plan_of(mcx, parse, "SELECT val + int4(random()), int8(val + int4(random())), count(*) FROM t GROUP BY 1, 2");
    assert!(agg_keys(&plan).is_some_and(|a| a.ends_with("Agg{2}")), "{:?}", chain(&plan));
}

#[test]
fn qual_evaluation_classes_order_strictly() {
    use costsize::pgrc_qual_cost_multiplier as m;
    use costsize::QualOp::{Eq, Neq, Other};
    let order = [m(2 | 1, Eq), m(1, Eq), m(1, Neq), m(4, Eq), m(4, Other), m(0, Eq)];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    assert_eq!(m(1, Neq), m(1, Other), "`<>` on a zone column is not zone-decidable (flag_reason <> 0)");
    assert_eq!(m(4, Neq), m(4, Eq), "`<>` on a dictionary column is a code compare (notes <> '')");
    assert_eq!(m(2 | 1, Neq), m(1, Neq), "a bloom serves equality only");
    assert_eq!(m(0, Other), 1.0, "no class: PostgreSQL's cost");
    assert_eq!(m(1 | 4, Other), m(1, Other), "zone outranks dictionary");
}

// Group-count bounds for expression keys — extract's field domain, date_trunc
// over the column's range in the unit, length ≤ the footer's max length else the typmod (a
// regexp_replace key takes PostgreSQL's reading: the Var's own ndistinct). The Var fixtures carry no
// timestamp column, so the rules are pinned here and the estimates in the server rig.
#[test]
fn expression_key_group_bounds() {
    use crate::selfuncs::{date_trunc_groups, extract_field_domain, length_groups};
    assert_eq!(extract_field_domain("minute"), Some(60.0));
    assert_eq!(extract_field_domain("MINUTE"), Some(60.0));
    assert_eq!(extract_field_domain("hour"), Some(24.0));
    assert_eq!(extract_field_domain("dow"), Some(7.0));
    assert_eq!(extract_field_domain("year"), None, "unbounded fields fall to the column's ndistinct");
    assert_eq!(extract_field_domain("epoch"), None);
    let two_days = Some((0.0, 2.0 * 86_400e6 - 60e6));
    assert_eq!(date_trunc_groups("minute", two_days), Some(2880.0), "two days of minutes");
    assert_eq!(date_trunc_groups("hour", two_days), Some(48.0));
    assert_eq!(date_trunc_groups("day", two_days), Some(2.0));
    assert_eq!(date_trunc_groups("minute", None), None, "no range: no bound");
    assert_eq!(date_trunc_groups("fortnight", two_days), None);
    assert_eq!(length_groups(-1, 94), Some(95.0), "footer max length");
    assert_eq!(length_groups(255 + 4, 0), Some(256.0), "varchar(255) typmod");
    assert_eq!(length_groups(-1, 0), None, "text without a footer fact");
}

// D3142-B: an expression key's group count is capped by its provable distinct-value bound (selfuncs
// expr_distinct_bound): the rules' domains, then planned estimates over the fixture's `val` (no statistics:
// PostgreSQL's 200 default groups) and `pk`.
#[test]
fn d3142b_rule_domains() {
    use crate::selfuncs::{extract_time_field_domain, int_mod_domain, width_bucket_domain};
    assert_eq!(int_mod_domain(10, false), Some(19.0), "-9..9");
    assert_eq!(int_mod_domain(-10, false), Some(19.0), "the sign follows the dividend");
    assert_eq!(int_mod_domain(10, true), Some(10.0), "0..9 for a non-negative dividend");
    assert_eq!(int_mod_domain(0, false), None, "x % 0 raises");
    assert_eq!(width_bucket_domain(10), Some(12.0), "buckets 0..=11");
    assert_eq!(width_bucket_domain(0), None);
    assert_eq!(extract_time_field_domain("hour"), Some(25.0), "time '24:00:00' has hour 24");
    assert_eq!(extract_time_field_domain("MINUTE"), Some(60.0));
    assert_eq!(extract_time_field_domain("second"), None);
}

const INT4MOD_OP: u32 = 530;
const INT4MOD_PROC: u32 = 156;
const INT4GT_PROC: u32 = 147;

fn op<'m>(mcx: Mcx<'m>, opno: u32, opfuncid: u32, rettype: u32, l: Node<'m>, r: Node<'m>) -> Node<'m> {
    Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno,
            opfuncid,
            opresulttype: rettype,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, l, r).unwrap(),
            location: -1,
        },
    )
    .unwrap()
}

fn int4c(mcx: Mcx<'_>, v: i32) -> Node<'_> {
    Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(v), false, true).unwrap()
}

/// `CASE WHEN cond THEN 1 [ELSE 2] END` (no ELSE: the parser's NULL default result).
fn case_of<'m>(mcx: Mcx<'m>, cond: Node<'m>, with_else: bool) -> Node<'m> {
    let when = Node::mk(mcx, types_nodes::primnodes::CaseWhen { expr: Some(cond), result: Some(int4c(mcx, 1)), location: -1 }).unwrap();
    Node::mk(
        mcx,
        types_nodes::primnodes::CaseExpr {
            casetype: INT4OID,
            args: NodeList::make1(mcx, when).unwrap(),
            defresult: Some(if with_else { int4c(mcx, 2) } else { Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(0), true, true).unwrap() }),
            location: -1,
            ..Default::default()
        },
    )
    .unwrap()
}

fn groups_of(key: for<'m> fn(Mcx<'m>) -> Node<'m>) -> f64 {
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(key(mcx), "k", 1), (count_star(mcx), "count", 0)], &[1], None);
    let plan = plan_of(mcx, parse, "SELECT k, count(*) FROM t GROUP BY k");
    let mut cur = Some(plan);
    while let Some(p) = cur {
        if let Some(a) = p.as_agg() {
            return a.plan.plan_rows;
        }
        cur = p.as_plan().unwrap().lefttree;
    }
    panic!("no Agg in {:?}", chain(&plan));
}

fn key_mod(mcx: Mcx<'_>) -> Node<'_> {
    op(mcx, INT4MOD_OP, INT4MOD_PROC, INT4OID, var(mcx, 2), int4c(mcx, 10))
}

fn key_int8_of_mod(mcx: Mcx<'_>) -> Node<'_> {
    Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 481, funcresulttype: INT8OID, args: NodeList::make1(mcx, key_mod(mcx)).unwrap(), ..Default::default() })
        .unwrap()
}

fn key_case_else(mcx: Mcx<'_>) -> Node<'_> {
    case_of(mcx, op(mcx, INT4_GT_OP, INT4GT_PROC, 16, var(mcx, 2), int4c(mcx, 5)), true)
}

fn key_case_no_else(mcx: Mcx<'_>) -> Node<'_> {
    case_of(mcx, op(mcx, INT4_GT_OP, INT4GT_PROC, 16, var(mcx, 2), int4c(mcx, 5)), false)
}

fn key_case_two_vars(mcx: Mcx<'_>) -> Node<'_> {
    case_of(mcx, op(mcx, INT4_GT_OP, INT4GT_PROC, 16, var(mcx, 2), var(mcx, 1)), true)
}

fn key_volatile_mod(mcx: Mcx<'_>) -> Node<'_> {
    let random = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 1598, funcresulttype: 701, ..Default::default() }).unwrap();
    let r = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 319, funcresulttype: INT4OID, args: NodeList::make1(mcx, random).unwrap(), ..Default::default() })
        .unwrap();
    op(mcx, INT4MOD_OP, INT4MOD_PROC, INT4OID, r, int4c(mcx, 10))
}

#[test]
fn d3142b_expression_keys_take_their_bound() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(groups_of(key_mod), 20.0, "val % 10: -9..9 and NULL, under the default 200");
    assert_eq!(groups_of(key_int8_of_mod), 20.0, "a function of a bounded argument keeps its bound");
    assert_eq!(groups_of(key_case_else), 2.0, "CASE: one value a branch");
    assert_eq!(groups_of(key_case_no_else), 2.0, "a missing ELSE adds its NULL");
    assert_eq!(groups_of(key_case_two_vars), 2.0, "a key over two columns: the product cap");
    let pg = groups_of(key_volatile_mod);
    assert!(pg > 20.0, "a volatile key has no bound: {pg}");
}

#[test]
fn d3142b_switch_off_keeps_postgres_reading() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::selfuncs::EXPR_GROUP_BOUND_OFF_FOR_TESTS.store(true, std::sync::atomic::Ordering::Relaxed);
    let off = groups_of(key_mod);
    crate::selfuncs::EXPR_GROUP_BOUND_OFF_FOR_TESTS.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(off, 200.0, "val's default 200 groups");
    assert_eq!(groups_of(key_mod), 20.0);
}

// PL-10 / PL-11 (gap-optoverfit GROUP-NDV-EST): a never-ANALYZEd columnar column's group-key ndistinct is the part
// facts' whole-table count clamped to the rel's tuples, at any scale; a column without that count keeps PostgreSQL's
// unknown (None here: get_variable_numdistinct's DEFAULT_NUM_DISTINCT stands), never a share of the table.
#[test]
fn columnar_group_ndistinct_is_the_facts_count_or_postgres_unknown() {
    use crate::selfuncs::columnar_group_ndistinct;
    assert_eq!(columnar_group_ndistinct(Some(6_000), 60e6), Some(6_000.0), "the sketch, not 5 % of 60M");
    assert_eq!(columnar_group_ndistinct(Some(6_000), 6e9), Some(6_000.0), "the table's size does not move it");
    assert_eq!(columnar_group_ndistinct(Some(900), 500.0), Some(500.0), "clamped to the rel's tuples");
    assert_eq!(columnar_group_ndistinct(Some(1), 1e6), Some(1.0));
    for tuples in [10.0, 1e4, 60e6, 6e9] {
        assert_eq!(columnar_group_ndistinct(None, tuples), None, "no count at {tuples} tuples: PostgreSQL's default");
        assert_eq!(columnar_group_ndistinct(Some(0), tuples), None, "an absent sketch serves 0");
    }
    assert_eq!(columnar_group_ndistinct(Some(6_000), 0.0), None, "an empty rel");
    assert_eq!(columnar_group_ndistinct(Some(6_000), f64::NAN), None);
}

#[test]
fn group_by_key_order_by_key_limit_plans_hashed_grouping_under_sort() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(
        mcx,
        None,
        &[(var(mcx, 2), "val", 1), (count_star(mcx), "count", 0)],
        &[1],
        Some((1, INT4EQ_OP, INT4_LT_OP, false)),
    );
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "SELECT val, count(*) FROM t GROUP BY val ORDER BY val LIMIT 10");
    assert_chain(&plan, &["Limit", "Sort", "HashAgg{1}", "Scan"]);
}

// D4169: `SELECT pk + 1, sum(DISTINCT val), row_number() OVER () FROM t WHERE pk + 1 = 5 GROUP BY pk + 1`. The
// EquivalenceClass makes the GROUP BY item redundant, so the hashed DISTINCT form regroups by the item itself
// (hashed_distinct_regroups), a group column of its input. The window input target flattened the item to `pk`, a
// column that input does not carry, and setrefs raised "variable not found in subplan target list". It keeps the item
// whole there; where another entry reads `pk` (max(pk)) the item flattens as before.
fn window_over_regroup<'m>(mcx: Mcx<'m>, max_pk: bool) -> Query<'m> {
    use types_nodes::parsenodes::WindowClause;
    use types_nodes::primnodes::WindowFunc;
    let five = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(5), false, true).unwrap();
    let qual = Node::mk(
        mcx,
        types_nodes::primnodes::OpExpr {
            opno: INT4EQ_OP,
            opfuncid: INT4EQ_PROC,
            opresulttype: 16,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, plus_one(mcx, var(mcx, 1)), five).unwrap(),
            location: -1,
        },
    )
    .unwrap();
    let wf = Node::mk(mcx, WindowFunc { winfnoid: 3100, wintype: INT8OID, winref: 1, ..WindowFunc::default() }).unwrap();
    let mut tl = vec![(plus_one(mcx, var(mcx, 1)), "k", 1), (sum_of(mcx, var(mcx, 2), true), "sum", 0)];
    if max_pk {
        tl.push((max_of(mcx, var(mcx, 1)), "max", 0));
    }
    tl.push((wf, "rn", 0));
    let mut parse = query(mcx, Some(qual), &tl, &[1], None);
    let wc = Node::mk(
        mcx,
        WindowClause { frameOptions: types_nodes::rawnodes::FRAMEOPTION_DEFAULTS, winref: 1, ..WindowClause::default() },
    )
    .unwrap();
    parse.windowClause = NodeList::make1(mcx, wc).unwrap();
    parse.hasWindowFuncs = true;
    parse
}

/// The expression the scan computes for column `col` of `plan`'s output, read down through the nodes that pass their
/// input columns on (setrefs' OUTER_VAR references), from `plan`'s child.
fn scan_expr_of<'m>(plan: Node<'m>, col: i16) -> Node<'m> {
    let (mut node, mut col) = (plan.as_plan().unwrap().lefttree.expect("a child"), col);
    loop {
        let p = node.as_plan().unwrap();
        let e = p.targetlist.nth((col - 1) as usize).as_target_entry().unwrap().expr;
        match (e.as_var(), p.lefttree) {
            (Some(v), Some(child)) if v.varno == types_nodes::primnodes::OUTER_VAR => (node, col) = (child, v.varattno),
            _ => return e,
        }
    }
}

#[test]
fn d4169_window_keeps_a_regrouped_item_its_group_input_lacks() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let plan = plan_of(mcx, window_over_regroup(mcx, false), "SELECT pk + 1, sum(DISTINCT val), row_number() OVER () FROM t WHERE pk + 1 = 5 GROUP BY pk + 1");
    assert_eq!(plan.node_tag(), NodeTag::T_WindowAgg, "a WindowAgg over the group");
    let agg = plan.as_plan().unwrap().lefttree.unwrap();
    let a = agg.as_agg().expect("the group under the WindowAgg");
    let first = a.plan.targetlist.nth(0).as_target_entry().unwrap().expr.as_var().expect("the item, read from the input");
    let e = scan_expr_of(agg, first.varattno);
    assert_eq!(e.node_tag(), NodeTag::T_OpExpr, "the group hands on the item (pk + 1) its input computes, not pk");
}

#[test]
fn d4169_window_flattens_a_regrouped_item_whose_columns_the_input_holds() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _columnar = ColumnarPosture::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let plan = plan_of(mcx, window_over_regroup(mcx, true), "SELECT pk + 1, sum(DISTINCT val), max(pk), row_number() OVER () FROM t WHERE pk + 1 = 5 GROUP BY pk + 1");
    assert_eq!(plan.node_tag(), NodeTag::T_WindowAgg, "a WindowAgg over the group");
    let agg = plan.as_plan().unwrap().lefttree.unwrap();
    let a = agg.as_agg().expect("the group under the WindowAgg");
    let first = a.plan.targetlist.nth(0).as_target_entry().unwrap().expr.as_var().expect("a column of the input");
    let e = scan_expr_of(agg, first.varattno);
    assert_eq!(e.as_var().map(|v| v.varattno), Some(1), "flattened to pk, as before: max(pk) brings it into the input");
}
