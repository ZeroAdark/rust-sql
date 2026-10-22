use super::plan_shapes::{count_star, limit, plan_of, plus_one, plus_pk, query, setup, sgc, sum_of, tle, var};
use super::*;
use crate::gravity_paths::*;
use types_nodes::plannodes::GravityRewrite;
use types_nodes::primnodes::Aggref;

const G: u64 = 1_000;
const INT8EQ_OP: u32 = 410;
const COUNT_ANY: u32 = 2147;

/// A layout clustered by the key: granule g holds keys 10g..10g+9.
fn sorted_zones() -> Vec<(i64, i64)> {
    (0..G as i64).map(|g| (g * 10, g * 10 + 9)).collect()
}

fn unsorted_zones() -> Vec<(i64, i64)> {
    vec![(0, 10 * G as i64 - 1); G as usize]
}

struct Latch(Option<i32>);
impl Latch {
    fn hold(zones: Vec<(i16, Vec<(i64, i64)>)>, work_mem_kb: Option<i32>) -> Self {
        *TEST_FACTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(TestFacts { granules: G, zones });
        let saved = work_mem_kb.map(|kb| {
            let old = guc_tables::vars::work_mem.read();
            guc_tables::vars::work_mem.write(kb);
            old
        });
        Latch(saved)
    }
}
impl Drop for Latch {
    fn drop(&mut self) {
        *TEST_FACTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(old) = self.0 {
            guc_tables::vars::work_mem.write(old);
        }
    }
}

/// ORDER_FREE_AGGS against PostgreSQL 18.6's pg_proc.dat: every name is a built-in aggregate, the set covers the
/// aggregates the OID list it replaced held (count, sum, avg, min, max, bool_and / bool_or / every, bit_and / bit_or),
/// and every other built-in aggregate is listed here as not order-free, so a new one is a decision, not a default.
#[test]
fn order_free_aggregates_match_pg_proc() {
    let dat = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../postgres-18.6-reference/src/include/catalog/pg_proc.dat");
    let text = std::fs::read_to_string(&dat).unwrap_or_else(|e| panic!("{}: {e}", dat.display()));
    let aggs: Vec<(u32, String)> = super::plan_shapes::dat_entries(&text)
        .into_iter()
        .filter(|e| e.get("prokind").is_some_and(|k| k == "a"))
        .map(|e| (e["oid"].parse().unwrap(), e["proname"].clone()))
        .collect();
    let names: std::collections::BTreeSet<&str> = aggs.iter().map(|a| a.1.as_str()).collect();
    for n in ORDER_FREE_AGGS {
        assert!(names.contains(n), "{n}: not a built-in aggregate");
    }
    let replaced = |o: u32| matches!(o, 2803 | 2147 | 2100..=2146 | 2517..=2519 | 2236..=2239);
    for (oid, name) in &aggs {
        assert!(!replaced(*oid) || ORDER_FREE_AGGS.contains(&name.as_str()), "{name} ({oid}) was order-free");
    }
    #[rustfmt::skip]
    let mut order_kept = [
        "any_value", "array_agg", "json_agg", "json_agg_strict", "json_object_agg", "json_object_agg_strict",
        "json_object_agg_unique", "json_object_agg_unique_strict", "jsonb_agg", "jsonb_agg_strict", "jsonb_object_agg",
        "jsonb_object_agg_strict", "jsonb_object_agg_unique", "jsonb_object_agg_unique_strict", "string_agg", "xmlagg",
        "cume_dist", "dense_rank", "mode", "percent_rank", "percentile_cont", "percentile_disc", "rank",
        "range_agg", "range_intersect_agg",
    ];
    order_kept.sort_unstable();
    let rest: Vec<&str> = names.iter().copied().filter(|n| !ORDER_FREE_AGGS.contains(n)).collect();
    assert_eq!(rest, order_kept, "built-in aggregates outside ORDER_FREE_AGGS");
}


/// Sorted layout: the n-th smallest minimum bounds exactly n granules
/// ascending, the n-th largest maximum exactly n descending; an all-null
/// granule (`min > max`) is never a survivor and not counted as keyed.
#[test]
fn census_on_a_sorted_layout_reads_n_granules() {
    let mut z = sorted_zones();
    z.push((i64::MAX, i64::MIN));
    let asc = zone_walk_census(&z, 10, false, 1).unwrap();
    assert_eq!(asc, ZoneCensus { survivors: 10, keyed: G, granules: G + 1 });
    let desc = zone_walk_census(&z, 10, true, 1).unwrap();
    assert_eq!(desc, ZoneCensus { survivors: 10, keyed: G, granules: G + 1 });
    assert_eq!(zone_walk_census(&z, 1, false, 1).unwrap().survivors, 1);
    assert_eq!(zone_walk_census(&z, 5_000, false, 1).unwrap().survivors, G, "n beyond the granules reads them all");
    assert!(zone_walk_census(&z, 0, false, 1).is_none());
    assert!(zone_walk_census(&[], 3, false, 1).is_none());
    assert!(zone_walk_census(&[(5, 1)], 3, false, 1).is_none(), "only all-null granules");
}

/// Unsorted layout: every minimum is the domain's minimum, so the bound
/// admits every granule — the path is not built (the walk reads every granule).
#[test]
fn census_on_an_unsorted_layout_reads_everything() {
    let z = unsorted_zones();
    let c = zone_walk_census(&z, 10, false, 1).unwrap();
    assert_eq!(c.survivors, G);
    assert_eq!(c.survivors, c.granules);
    assert_eq!(zone_walk_census(&z, 10, true, 1).unwrap().survivors, G);
}

#[test]
fn nulls_first_census_counts_the_granules_holding_a_null() {
    let rows = 8_192.0;
    let mut z = sorted_zones();
    assert_eq!(nulls_first_census(&z, 10, false, 8_192, rows, 0.0, 0.0), zone_walk_census(&z, 10, false, 8_192), "no NULLs: the keyed census");
    let spread = nulls_first_census(&z, 10, false, 8_192, rows, G as f64 * rows / 1000.0, G as f64 * rows / 1000.0).unwrap();
    assert_eq!(spread.survivors, 1, "the fewest granules the NULLs fill");
    let wider = nulls_first_census(&z, 10, false, 8_192, rows, 40.0 * rows, 0.0).unwrap();
    assert_eq!(wider.survivors, 41, "forty granules of NULLs and the keyed census's one");
    z.extend(std::iter::repeat((i64::MAX, i64::MIN)).take(8));
    let held = nulls_first_census(&z, 10, false, 8_192, rows, 8.0 * rows, 0.0).unwrap();
    assert_eq!(held, ZoneCensus { survivors: 9, keyed: G, granules: G + 8 });
    let noisy = nulls_first_census(&z, 10, false, 8_192, rows, 8.0 * rows + 1_573.0, 0.0).unwrap();
    assert_eq!(noisy.survivors, 10);
    let filled = nulls_first_census(&z, 10, false, 8_192, rows, 8.0 * rows, 8.0 * rows).unwrap();
    assert_eq!(filled.survivors, 8);
    let few = nulls_first_census(&sorted_zones(), 10, true, 8_192, rows, 5.0, 5.0).unwrap();
    assert_eq!(few.survivors, 2, "{few:?}");
    assert!(nulls_first_census(&[(5, 1)], 3, false, 1, rows, 1.0, 1.0).is_none(), "only all-null granules");
    assert!(nulls_first_census(&z, 0, false, 1, rows, 0.0, 0.0).is_none());
    assert_eq!(nulls_first_census(&z, 10, false, 8_192, rows, f64::NAN, -3.0), nulls_first_census(&z, 10, false, 8_192, rows, 0.0, 0.0));
}

#[test]
fn census_directions_and_ties() {
    // Three granules share the smallest minimum; descending, the maxima differ.
    let z = vec![(0, 100), (0, 50), (0, 20), (30, 40), (60, 70)];
    assert_eq!(zone_walk_census(&z, 1, false, 1).unwrap().survivors, 3, "ties at the bound all survive");
    assert_eq!(zone_walk_census(&z, 4, false, 1).unwrap().survivors, 4);
    assert_eq!(zone_walk_census(&z, 1, true, 1).unwrap().survivors, 1, "max 100 alone");
    assert_eq!(zone_walk_census(&z, 2, true, 1).unwrap().survivors, 2, "max 100, 70");
}

/// a granule hands the walk its share of the scan's rows, so a clustered layout's bound is its first granule's
/// far side, not its n-th edge: n = 15 at 8,192 rows a granule reads one granule (the one-key proxy read 15, and
/// past half the granules declined the path), in either physical order and either direction. One key a granule is the
/// old proxy exactly.
#[test]
fn census_counts_each_granules_rows() {
    let z = sorted_zones();
    let mut rev = z.clone();
    rev.reverse();
    for layout in [&z, &rev] {
        assert_eq!(zone_walk_census(layout, 15, false, 1).unwrap().survivors, 15, "one key a granule: the n-th minimum");
        assert_eq!(zone_walk_census(layout, 15, false, 8_192).unwrap().survivors, 1, "the first granule holds the 15 rows");
        assert_eq!(zone_walk_census(layout, 15, true, 8_192).unwrap().survivors, 1, "descending: the last granule");
        assert_eq!(zone_walk_census(layout, 600, false, 10).unwrap().survivors, 60, "60 granules of 10 rows hold 600");
    }
    let lap: Vec<(i64, i64)> = (0..G as i64).map(|g| (g * 10, g * 10 + 15)).collect();
    assert_eq!(zone_walk_census(&lap, 15, false, 8_192).unwrap().survivors, 2);
    assert_eq!(zone_walk_census(&unsorted_zones(), 15, false, 8_192).unwrap().survivors, G);
    assert_eq!(zone_walk_census(&z[..5], 100, false, 10).unwrap().survivors, 5);
    assert_eq!(zone_walk_census(&z, 15, false, 0), zone_walk_census(&z, 15, false, 1), "no estimate: one key a granule");
}


#[test]
fn two_pass_passes_cost_reads_the_input_twice() {
    assert_eq!(two_pass_passes_cost(1000.0, 10_000.0, 0.0025), 2.0 * (1000.0 + 25.0));
    assert_eq!(two_pass_passes_cost(0.0, 0.0, 0.0025), 0.0);
}

#[test]
fn two_pass_candidates_are_the_repeats_plus_n() {
    let cand = two_pass_candidates(99_997_497.0, 99_661_990.0, 10.0);
    assert!((cand - (99_997_497.0 - 99_661_990.0 + 10.0)).abs() < 1e-6, "{cand}");
    assert_eq!(two_pass_candidates(1e6, 1e6, 10.0), 10.0);
    // ndv above the rows (a stale estimate): still n, never negative.
    assert_eq!(two_pass_candidates(1e6, 2e6, 10.0), 10.0);
    // 23 values over a million rows: all but the 23 first rows repeat, plus n, never more than the rows.
    assert_eq!(two_pass_candidates(1e6, 23.0, 10.0), 1e6 - 23.0 + 10.0);
    assert_eq!(two_pass_candidates(1e6, 1.0, 10.0), 1e6);
    assert_eq!(two_pass_candidates(0.0, 0.0, 10.0), 0.0);
}

#[test]
fn simple_aggregate_arguments_are_var_const_or_relabel() {
    let cx = setup();
    let mcx = cx.mcx();
    let v = var(mcx, 2);
    assert!(simple_agg_arg(v));
    let c = Node::mk_const(mcx, 23, -1, 0, 4, Datum::from_i32(1), false, true).unwrap();
    assert!(simple_agg_arg(c));
    let relabel = Node::mk(mcx, types_nodes::primnodes::RelabelType { arg: v, resulttype: 23, resulttypmod: -1, resultcollid: 0, relabelformat: types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
    assert!(simple_agg_arg(relabel));
    assert!(!simple_agg_arg(plus_one(mcx, v)), "an expression may raise on a non-candidate row");
    let relabel_expr = Node::mk(mcx, types_nodes::primnodes::RelabelType { arg: plus_one(mcx, v), resulttype: 23, resulttypmod: -1, resultcollid: 0, relabelformat: types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
    assert!(!simple_agg_arg(relabel_expr));
}

#[test]
fn order_insensitive_aggregates_and_word_keys() {
    install_fixtures();
    for f in [2803u32, 2147, 2108, 2101, 2116, 2132, 2517, 2240 /* bit_and(int8) */, 2112 /* sum(money) */] {
        assert!(order_insensitive_agg(f).unwrap(), "{f}");
    }
    for f in [2335u32 /* array_agg */, 3538 /* string_agg */, 3972 /* percentile_disc */, 99_999 /* no such */] {
        assert!(!order_insensitive_agg(f).unwrap(), "{f}");
    }
    // typesem's word order verdict: the integers, date, time, the timestamps, bool, and money / "char" /
    // oid (signed, or unsigned narrower than a word); not text, numeric, float8, uuid, xid8 or pg_lsn
    // (the fixture's pg_type holds a subset: a type it lacks is a lookup error here, never a verdict; the e2e witness
    // stats_pl05_pl06_word_types runs money / oid / "char" over the catalog)
    let mut read = 0;
    for (t, walks) in [(21u32, true), (23, true), (20, true), (1082, true), (1083, true), (1114, true), (1184, true), (16, true), (790, true), (18, true), (26, true), (25, false), (1700, false), (701, false), (2950, false), (5069, false), (3220, false)] {
        if let Ok(got) = zone_word_type(t) {
            assert_eq!(got, walks, "{t}");
            read += 1;
        }
    }
    assert!(read >= 4, "the fixture answers at least the integers");
}

/// a walk's sort key must sort by its type's default btree order (the zones' and the word image's): a
/// pathkey under another operator family (a user's reverse opclass over int8) is no walk.
#[test]
fn zone_walks_take_the_default_btree_order_only() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _cx = setup();
    let pk = |fam: u32| types_pathnodes::PathKey { pk_eclass: None, pk_opfamily: fam, pk_cmptype: types_pathnodes::COMPARE_LT, pk_nulls_first: false };
    let int_fam = typcache::lookup_type_cache(23, typcache::TYPECACHE_BTREE_OPFAMILY).unwrap().btree_opf();
    assert_ne!(int_fam, 0, "int4 has a default btree opfamily");
    assert!(crate::gravity_paths::default_btree_order(&pk(int_fam), 23).unwrap());
    assert!(!crate::gravity_paths::default_btree_order(&pk(int_fam + 100_000), 23).unwrap(), "another family");
}


fn agg_rewrite(plan: &Node<'_>) -> Option<GravityRewrite> {
    let mut cur = Some(*plan);
    while let Some(p) = cur {
        if let Some(a) = p.as_agg() {
            return a.gravity_rewrite;
        }
        cur = p.as_plan().unwrap().lefttree;
    }
    panic!("no Agg in the plan");
}

fn sort_rewrite(plan: &Node<'_>) -> Option<GravityRewrite> {
    let mut cur = Some(*plan);
    while let Some(p) = cur {
        if p.node_tag() == NodeTag::T_Sort {
            return p.as_variant::<types_nodes::plannodes::Sort>().unwrap().gravity_rewrite;
        }
        cur = p.as_plan().unwrap().lefttree;
    }
    panic!("no Sort in the plan");
}

fn tags(plan: &Node<'_>) -> Vec<NodeTag> {
    let mut out = Vec::new();
    let mut cur = Some(*plan);
    while let Some(p) = cur {
        out.push(p.node_tag());
        cur = p.as_plan().unwrap().lefttree;
    }
    out
}

fn group_limit_query(mcx: Mcx<'_>, order: Option<(u32, u32, u32, bool)>) -> Query<'_> {
    let mut parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (count_star(mcx), "count", 0)], &[1], order);
    limit(&mut parse, mcx, 5);
    parse
}

/// Item 3 (a): the zone top-k path is built for `GROUP BY k LIMIT n` without
/// ORDER BY over a clustered key, annotated `asc nulls last` on the key's
/// attno; not on an unsorted layout, not without facts, not with ORDER BY.
#[test]
fn zone_topk_is_built_on_a_sorted_layout_only() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    {
        let _l = Latch::hold(vec![(2, sorted_zones())], None);
        let plan = plan_of(mcx, group_limit_query(mcx, None), "SELECT val, count(*) FROM t GROUP BY val LIMIT 5");
        assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Agg, NodeTag::T_SeqScan]);
        assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::ZoneTopK { attno: 2, desc: false, nulls_first: false }));
        assert_eq!(plan.as_plan().unwrap().lefttree.unwrap().as_agg().unwrap().aggstrategy, types_pathnodes::AGG_HASHED);
        let plan = plan_of(mcx, group_limit_query(mcx, Some((1, INT4EQ_OP, INT4_LT_OP, false))), "… ORDER BY val LIMIT 5");
        assert_eq!(agg_rewrite(&plan), None);
    }
    {
        let _l = Latch::hold(vec![(2, unsorted_zones())], None);
        let plan = plan_of(mcx, group_limit_query(mcx, None), "unsorted");
        assert_eq!(agg_rewrite(&plan), None, "every granule survives: no path");
    }
    {
        let _l = Latch::hold(vec![(1, sorted_zones())], None);
        let plan = plan_of(mcx, group_limit_query(mcx, None), "zones on another column");
        assert_eq!(agg_rewrite(&plan), None);
    }
    let plan = plan_of(mcx, group_limit_query(mcx, None), "heap, no facts");
    assert_eq!(agg_rewrite(&plan), None);
}

struct FittedTop;
impl FittedTop {
    fn hold() -> Self {
        *crate::gravity_paths::TEST_TOP_PROFILE.lock().unwrap_or_else(|e| e.into_inner()) = Some("fitted");
        FittedTop
    }
}
impl Drop for FittedTop {
    fn drop(&mut self) {
        *crate::gravity_paths::TEST_TOP_PROFILE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

struct NoIndex;
impl NoIndex {
    fn hold() -> Self {
        ::costsize::gucs::set_enable_indexscan(false);
        ::costsize::gucs::set_enable_indexonlyscan(false);
        ::costsize::gucs::set_enable_bitmapscan(false);
        NoIndex
    }
}
impl Drop for NoIndex {
    fn drop(&mut self) {
        ::costsize::gucs::set_enable_indexscan(true);
        ::costsize::gucs::set_enable_indexonlyscan(true);
        ::costsize::gucs::set_enable_bitmapscan(true);
    }
}

fn pk_group_query(mcx: Mcx<'_>, order: Option<bool>, n: i64) -> Query<'_> {
    let order = order.map(|desc| (1, INT4EQ_OP, if desc { INT4_GT_OP } else { INT4_LT_OP }, desc));
    let mut parse = query(mcx, None, &[(var(mcx, 1), "pk", 1), (count_star(mcx), "count", 0)], &[1], order);
    limit(&mut parse, mcx, n);
    parse
}

#[test]
fn zone_topk_is_built_under_an_order_by_its_key_either_way() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![(1, sorted_zones())], None);
    let (_w, _i) = (Width::set(0), NoIndex::hold());
    let plan = plan_of(mcx, pk_group_query(mcx, Some(false), 5), "SELECT pk, count(*) FROM t GROUP BY pk ORDER BY pk LIMIT 5");
    assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Sort, NodeTag::T_Agg, NodeTag::T_SeqScan]);
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::ZoneTopK { attno: 1, desc: false, nulls_first: false }));
    let plan = plan_of(mcx, pk_group_query(mcx, Some(true), 5), "SELECT pk, count(*) FROM t GROUP BY pk ORDER BY pk DESC LIMIT 5");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::ZoneTopK { attno: 1, desc: true, nulls_first: false }));
    let plan = plan_of(mcx, pk_group_query(mcx, None, 5), "SELECT pk, count(*) FROM t GROUP BY pk LIMIT 5");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::ZoneTopK { attno: 1, desc: false, nulls_first: false }));
}

#[test]
fn zone_topk_is_a_cost_decision_at_the_walks_price() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let (_l, _i) = (Latch::hold(vec![(1, sorted_zones())], None), NoIndex::hold());
    let walk = cost_profile::walk::Walk::of(cost_profile::walk::Profile::parse(None));
    let spread = crate::gravity_paths::zone_spread(&sorted_zones());
    assert!(spread < 0.01, "{spread}");
    let even = |w: u32| (1..G).take_while(|&n| walk.group_share(n, G, n, n, spread, 1.0, w) < 1.0).last().expect("a walk under its price") as f64;
    let (w0_even, w2_even, w8_even) = (even(0), even(2), even(8));
    let topk = Some(GravityRewrite::ZoneTopK { attno: 1, desc: false, nulls_first: false });
    for order in [None, Some(false)] {
        for (width, n, walked) in [(0, 0.5 * w8_even, true), (0, 0.9 * w0_even, true), (0, 1.1 * w0_even, false), (2, 0.9 * w2_even, true), (2, 1.1 * w2_even, false), (8, 0.5 * w8_even, true), (8, 1.5 * w8_even, false)] {
            let _w = Width::set(width);
            let plan = plan_of(mcx, pk_group_query(mcx, order, n as i64), "SELECT pk, count(*) FROM t GROUP BY pk [ORDER BY pk] LIMIT n");
            assert_eq!(agg_rewrite(&plan), walked.then_some(topk).flatten(), "LIMIT {n} at width {width}, order {order:?} (break-evens W0 {w0_even} W2 {w2_even} W8 {w8_even})");
        }
    }
}

fn nested_plan_of<'m>(mcx: Mcx<'m>, parse: Query<'m>) -> Node<'m> {
    let mut run = crate::run::PlannerRun::new(mcx);
    run.root.gravity_nested = true;
    crate::subquery::subquery_planner(&mut run, leak_q(mcx, parse), false, 0.0, None).unwrap();
    let final_rel = crate::planmain::fetch_final_rel(&mut run);
    let best = crate::pathnode::get_cheapest_fractional_path(&run, final_rel, 0.0);
    crate::createplan::create_plan(&mut run, best).unwrap()
}

#[test]
fn a_nested_level_prices_the_group_walk_at_width_0() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let (_l, _i) = (Latch::hold(vec![(1, sorted_zones())], None), NoIndex::hold());
    let walk = cost_profile::walk::Walk::of(cost_profile::walk::Profile::parse(None));
    let spread = crate::gravity_paths::zone_spread(&sorted_zones());
    let even = |w: u32| (1..G).take_while(|&n| walk.group_share(n, G, n, n, spread, 1.0, w) < 1.0).last().expect("a walk under its price") as f64;
    let (w0_even, w8_even) = (even(0), even(8));
    assert!(w8_even * 1.5 < w0_even * 0.9, "W8 break-even {w8_even} under W0's {w0_even}");
    let n = ((w8_even * 1.5 + w0_even * 0.9) / 2.0) as i64;
    let topk = Some(GravityRewrite::ZoneTopK { attno: 1, desc: false, nulls_first: false });
    let _w = Width::set(8);
    for order in [None, Some(false)] {
        let top = plan_of(mcx, pk_group_query(mcx, order, n), "SELECT pk, count(*) FROM t GROUP BY pk [ORDER BY pk] LIMIT n");
        assert_eq!(agg_rewrite(&top), None, "the top level at width 8, LIMIT {n}, order {order:?} (W0 {w0_even} W8 {w8_even})");
        let nested = nested_plan_of(mcx, pk_group_query(mcx, order, n));
        assert_eq!(agg_rewrite(&nested), topk, "a nested level at width 8, LIMIT {n}, order {order:?} (W0 {w0_even} W8 {w8_even})");
    }
    let mut run = crate::run::PlannerRun::new(mcx);
    run.root.gravity_nested = true;
    run.push_root().unwrap();
    assert!(run.root.gravity_nested, "a level planned under a nested one is nested");
    assert!(run.root.make_minmax_subroot().gravity_nested, "a minmax subroot of a nested level is nested");
}

fn order_limit_query(mcx: Mcx<'_>, sortop: u32, desc: bool) -> Query<'_> {
    let mut parse = query(mcx, None, &[(var(mcx, 1), "pk", 0), (var(mcx, 2), "val", 1)], &[], Some((1, INT4EQ_OP, sortop, desc)));
    limit(&mut parse, mcx, 2);
    parse
}

/// NEW-stats-2: the NULLS FIRST census reads the facts' exact null fraction where the relation carries one, in place of
/// the statistics' (none in the fixture): a key nine rows in ten NULL fills at least nine granules in ten with NULLs the
/// walk reads first, so `ORDER BY val DESC LIMIT 2` (NULLS FIRST) builds no walk; without the fraction it builds one.
#[test]
fn nulls_first_census_reads_the_facts_null_fraction() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![(2, sorted_zones())], None);
    let plan = plan_of(mcx, order_limit_query(mcx, INT4_GT_OP, true), "SELECT pk FROM t ORDER BY val DESC LIMIT 2");
    assert_eq!(sort_rewrite(&plan), Some(GravityRewrite::ZoneWalk { attno: 2, desc: true, nulls_first: true }));
    *crate::gravity_paths::TEST_NULL_FRAC.lock().unwrap_or_else(|e| e.into_inner()) = Some((2, 0.9));
    let plan = plan_of(mcx, order_limit_query(mcx, INT4_GT_OP, true), "SELECT pk FROM t ORDER BY val DESC LIMIT 2");
    *crate::gravity_paths::TEST_NULL_FRAC.lock().unwrap_or_else(|e| e.into_inner()) = None;
    assert_eq!(sort_rewrite(&plan), None, "nine rows in ten NULL fill nine granules in ten first: no walk");
}

/// Item 3 (b): the zone walk is built for `ORDER BY k LIMIT n` over a
/// clustered key in both directions with the key's null placement; a
/// tie-breaker key is allowed; an unsorted layout and a missing fact build
/// nothing.
#[test]
fn zone_walk_is_built_in_both_directions() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    {
        let _l = Latch::hold(vec![(2, sorted_zones())], None);
        let plan = plan_of(mcx, order_limit_query(mcx, INT4_LT_OP, false), "SELECT pk FROM t ORDER BY val LIMIT 2");
        assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Sort, NodeTag::T_SeqScan]);
        assert_eq!(sort_rewrite(&plan), Some(GravityRewrite::ZoneWalk { attno: 2, desc: false, nulls_first: false }));
        let plan = plan_of(mcx, order_limit_query(mcx, INT4_GT_OP, true), "SELECT pk FROM t ORDER BY val DESC LIMIT 2");
        assert_eq!(sort_rewrite(&plan), Some(GravityRewrite::ZoneWalk { attno: 2, desc: true, nulls_first: true }));
        let mut parse = order_limit_query(mcx, INT4_LT_OP, false);
        let mut tl = NodeList::nil();
        tl.lappend(mcx, tle(mcx, var(mcx, 1), 1, "pk", 2)).unwrap();
        tl.lappend(mcx, tle(mcx, var(mcx, 2), 2, "val", 1)).unwrap();
        parse.targetList = tl;
        parse.sortClause.lappend(mcx, sgc(mcx, 2, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
        let plan = plan_of(mcx, parse, "SELECT pk FROM t ORDER BY val, pk LIMIT 2");
        assert_eq!(sort_rewrite(&plan), Some(GravityRewrite::ZoneWalk { attno: 2, desc: false, nulls_first: false }));
    }
    {
        let _l = Latch::hold(vec![(2, sorted_zones())], None);
        let mut parse = order_limit_query(mcx, INT4_LT_OP, false);
        limit(&mut parse, mcx, 600);
        let plan = plan_of(mcx, parse, "SELECT pk FROM t ORDER BY val LIMIT 600");
        assert_eq!(sort_rewrite(&plan), Some(GravityRewrite::ZoneWalk { attno: 2, desc: false, nulls_first: false }));
    }
    {
        let _l = Latch::hold(vec![(2, unsorted_zones())], None);
        let plan = plan_of(mcx, order_limit_query(mcx, INT4_LT_OP, false), "unsorted");
        assert_eq!(sort_rewrite(&plan), None);
    }
    let plan = plan_of(mcx, order_limit_query(mcx, INT4_LT_OP, false), "heap, no facts");
    assert_eq!(sort_rewrite(&plan), None);
}

#[test]
fn zone_walk_is_a_cost_decision_at_the_walks_read_factor() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![(2, sorted_zones())], None);
    let _w = Width::set(0);
    let walk = cost_profile::walk::Walk::of(cost_profile::walk::Profile::parse(None));
    let even = (1..G).take_while(|&s| walk.share(s, G, 0, false) < 1.0).last().expect("a walk under its price") as f64 / G as f64;
    assert!(even > 0.1 && even < 0.9, "{even}");
    let below = ((even - 0.05) * 10_000.0) as i64;
    let above = ((even + 0.05).min(0.95) * 10_000.0) as i64;
    for (n, walked) in [(below, true), (above, false), (9_000, false)] {
        let mut parse = order_limit_query(mcx, INT4_LT_OP, false);
        limit(&mut parse, mcx, n);
        let plan = plan_of(mcx, parse, "SELECT pk FROM t ORDER BY val LIMIT n");
        let want = walked.then_some(GravityRewrite::ZoneWalk { attno: 2, desc: false, nulls_first: false });
        assert_eq!(sort_rewrite(&plan), want, "LIMIT {n} (break-even {even})");
    }
}

#[test]
fn zone_walk_at_a_width_takes_the_plain_read_past_the_width_terms_break_even() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![(2, sorted_zones())], None);
    let walk = cost_profile::walk::Walk::of(cost_profile::walk::Profile::parse(None));
    let last = |width: u32| (1..G).take_while(|&s| walk.share(s, G, width, false) < 1.0).last().expect("a walk under its price") as f64;
    let (w8_even, w0_even) = (last(8), last(0));
    assert!(walk.share(10, G, 8, false) > walk.share(10, G, 0, false), "a short walk pays its start at width 8");
    for width in [0, 8] {
        let even = if width >= 8 { w8_even.min(w0_even) } else { w0_even };
        for (granules, walked) in [((0.9 * even) as i64, true), ((1.05 * even).min(0.97 * G as f64) as i64, false)] {
            let _w = Width::set(width);
            let mut parse = order_limit_query(mcx, INT4_LT_OP, false);
            limit(&mut parse, mcx, granules * 10);
            let plan = plan_of(mcx, parse, "SELECT pk FROM t ORDER BY val LIMIT n");
            let want = walked.then_some(GravityRewrite::ZoneWalk { attno: 2, desc: false, nulls_first: false });
            assert_eq!(sort_rewrite(&plan), want, "{granules} of {G} granules at width {width} (break-evens W0 {w0_even} W8 {w8_even})");
        }
    }
}

fn count_of<'m>(mcx: Mcx<'m>, expr: Node<'m>, filter: Option<Node<'m>>) -> Node<'m> {
    let mut aggargtypes = types_nodes::list::OidList::nil();
    aggargtypes.lappend(mcx, 23).unwrap();
    let args = NodeList::make1(mcx, tle(mcx, expr, 1, "arg", 0)).unwrap();
    Node::mk(mcx, Aggref { aggfnoid: COUNT_ANY, aggtype: 20, aggargtypes, args, aggfilter: filter, ..Aggref::default() }).unwrap()
}

/// `SELECT pk, val + 1, <count>, [extra] FROM t GROUP BY pk, val + 1 ORDER BY <count> DESC [, pk dir] LIMIT 10`
/// (a near-unique leading key: `pk` is unique, and `val + 1` rather than `val`
/// because a plain `val` would be trimmed as dependent on the primary key, leaving one grouping
/// column where the predicate wants two).
fn two_pass_query<'m>(mcx: Mcx<'m>, count: Node<'m>, extra: Option<Node<'m>>, floor: Option<(u32, bool)>) -> Query<'m> {
    let mut tlist: Vec<(Node<'m>, &'m str, u32)> = vec![(var(mcx, 1), "pk", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (count, "c", 3)];
    if let Some(e) = extra {
        tlist.push((e, "extra", 0));
    }
    let mut parse = query(mcx, None, &tlist, &[1, 2], Some((3, INT8EQ_OP, INT8GT_OP, true)));
    if let Some((sortop, desc)) = floor {
        parse.sortClause.lappend(mcx, sgc(mcx, 1, INT4EQ_OP, sortop, desc)).unwrap();
    }
    limit(&mut parse, mcx, 10);
    parse
}

const SPILL_AT: Option<i32> = Some(100);

struct Width(i32);
impl Width {
    fn set(w: i32) -> Self {
        let old = crate::gucs::max_parallel_workers_per_gather();
        crate::gucs::set_max_parallel_workers_per_gather(w);
        Width(old)
    }
}
impl Drop for Width {
    fn drop(&mut self) {
        crate::gucs::set_max_parallel_workers_per_gather(self.0);
    }
}

#[test]
fn two_pass_is_built_with_the_floor_order() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _fitted = FittedTop::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![], SPILL_AT);
    let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, None), "SELECT pk, val + 1, count(*) c FROM t GROUP BY pk, val + 1 ORDER BY c DESC LIMIT 10");
    assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Sort, NodeTag::T_Agg, NodeTag::T_SeqScan]);
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: false }));
    assert_eq!(sort_rewrite(&plan), None, "the Sort above is plain");
    let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, Some((INT4_GT_OP, true))), "… ORDER BY c DESC, pk DESC LIMIT 10");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: true, floor_nulls_first: true }));
    let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, Some((INT4_LT_OP, false))), "… ORDER BY c DESC, pk LIMIT 10");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: false }));
    let mut parse = two_pass_query(mcx, count_star(mcx), None, None);
    let nf = sgc(mcx, 1, INT4EQ_OP, INT4_LT_OP, false);
    // SAFETY: freshly built clause; no other reference is live.
    unsafe { nf.with_mut::<types_nodes::parsenodes::SortGroupClause, _>(|c| c.nulls_first = true) }.unwrap();
    parse.sortClause.lappend(mcx, nf).unwrap();
    let plan = plan_of(mcx, parse, "… ORDER BY c DESC, pk NULLS FIRST LIMIT 10");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: true }));
    let _w = Width::set(0);
    let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, None), "W0: SELECT pk, val + 1, count(*) c FROM t GROUP BY pk, val + 1 ORDER BY c DESC LIMIT 10");
    assert_eq!(agg_rewrite(&plan), None, "W0: one pass");
}

#[test]
fn two_pass_rejections() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _fitted = FittedTop::hold();
    let cx = setup();
    let mcx = cx.mcx();
    {
        let _l = Latch::hold(vec![], SPILL_AT);
        let plan = plan_of(mcx, two_pass_query(mcx, count_of(mcx, var(mcx, 2), None), None, None), "count(val)");
        assert_eq!(agg_rewrite(&plan), None, "count(x)");
        let t = Node::mk_const(mcx, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
        let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), Some(count_of(mcx, var(mcx, 2), Some(t))), None), "FILTER");
        assert_eq!(agg_rewrite(&plan), None, "FILTER");
        let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), Some(sum_of(mcx, plus_pk(mcx, var(mcx, 2)), false)), None), "sum(val + pk)");
        assert_eq!(agg_rewrite(&plan), None, "non-Var argument");
        let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), Some(sum_of(mcx, var(mcx, 2), false)), None), "sum(val)");
        assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: false }), "a Var argument is admitted");
        // pass 2 projects only the admitted groups; PG projects every group before the Sort, so an entry that can
        // raise (`pk + 1`, not a grouping expression; `sum(val) + 1` over a final) takes the plain path.
        let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), Some(plus_one(mcx, var(mcx, 1))), None), "pk + 1");
        assert_eq!(agg_rewrite(&plan), None, "a projection that can raise");
        let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), Some(plus_one(mcx, sum_of(mcx, var(mcx, 2), false))), None), "sum(val) + 1");
        assert_eq!(agg_rewrite(&plan), None, "a projection over a final that can raise");
        let mut parse = two_pass_query(mcx, count_star(mcx), None, None);
        parse.sortClause.lappend(mcx, sgc(mcx, 2, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
        let plan = plan_of(mcx, parse, "… ORDER BY c DESC, val + 1");
        assert_eq!(agg_rewrite(&plan), None, "secondary key is not k1");
        let mut parse = two_pass_query(mcx, count_star(mcx), None, None);
        let one = Node::mk_const(mcx, 20, -1, 0, 8, Datum::from_i64(1), false, true).unwrap();
        let mut args = NodeList::make1(mcx, count_star(mcx)).unwrap();
        args.lappend(mcx, one).unwrap();
        parse.havingQual = Some(Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: INT8GT_OP, opfuncid: 470, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 }).unwrap());
        let plan = plan_of(mcx, parse, "HAVING count(*) > 1");
        assert_eq!(agg_rewrite(&plan), None, "HAVING");
        let mut parse = two_pass_query(mcx, count_star(mcx), None, None);
        let p = types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 20, paramtypmod: -1, paramcollid: 0, location: -1 };
        parse.limitCount = Some(Node::mk(mcx, p).unwrap());
        let plan = plan_of(mcx, parse, "… LIMIT $1");
        assert_eq!(agg_rewrite(&plan), None, "a LIMIT that is not a constant");
    }
    {
        let _l = Latch::hold(vec![], None);
        let tlist: Vec<(Node<'_>, &str, u32)> = vec![(var(mcx, 2), "val", 1), (plus_one(mcx, var(mcx, 1)), "pk1", 2), (count_star(mcx), "c", 3)];
        let mut parse = query(mcx, None, &tlist, &[1, 2], Some((3, INT8EQ_OP, INT8GT_OP, true)));
        limit(&mut parse, mcx, 10);
        let plan = plan_of(mcx, parse, "SELECT val, pk + 1, count(*) c FROM t GROUP BY val, pk + 1 ORDER BY c DESC LIMIT 10");
        assert_eq!(agg_rewrite(&plan), None, "a repeating leading key: one pass is cheaper");
    }
    let heap = agg_rewrite(&plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, None), "heap"));
    let facts = {
        let _l = Latch::hold(vec![], None);
        agg_rewrite(&plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, None), "facts"))
    };
    assert_eq!(heap, facts, "facts do not change the two-pass choice");
}

/// D2078's cell size in PostgreSQL's units: 2 M heap rows (one read 35 600), `GROUP BY g, j` (666 k groups over 333 k
/// leading keys, 3 rows a group, `g` clustered in the scan's order), `ORDER BY min(v) LIMIT 20` over a nullable `v`
/// (NULLS LAST: 120 candidate rows in 40 groups), one transition a row, `work_mem` 4 MB (8 MB of hash memory a
/// participant).
fn d2078_cell() -> TopCostIn {
    TopCostIn {
        scan: 35_600.0,
        rows: 2e6,
        groups: 666e3,
        keys: 333e3,
        cand: 120.0,
        cand_groups: 40.0,
        n: 20.0,
        fold_row: 0.0025 + 2.0 * 0.0025,
        fold_group: 0.01,
        merge_row: 0.0025 + 2.0 * 0.0025,
        cpu_op: 0.0025,
        pass1_record: pass1_extreme_record_bytes(false, true, false),
        hash_mem: 8.0 * 1024.0 * 1024.0,
        key_corr: Some(1.0),
        grant: 0.0,
        width: 0.0,
        group_bytes: group_record_bytes(2, 1),
        t1_record: phase1_record_bytes(2, 1, 1),
        fold_key: 2.0 * 0.0025,
        groups_met: 666e3,
        w0_spill_row: 0.0,
        byref_state_bytes: 0.0,
    }
}

/// at width 0 a count top whose one pass outgrows the leader's hash memory pays PostgreSQL's hashed-Agg spill
/// on every row, and the two passes over a held pass 1 pay it only on their candidates (none when those fit): H015's
/// shape (W0 one pass 37-40 s spilling vs 182 ms two passes at W8) now prices the two passes below the one pass; with the
/// spill price 0 (the switch off) the reading is the old one.
#[test]
fn a_width_0_one_pass_that_spills_pays_postgres_spill() {
    let c = TopCostIn { keys: 1e3, key_corr: None, cand: 2e4, cand_groups: 2e3, groups: 5e6, groups_met: 5e6, pass1_record: pass1_record_bytes(false), w0_spill_row: 0.05, ..d2078_cell() };
    assert!(c.pass1_holds() && !c.one_pass_holds(), "pass 1's 1 k keys hold, the one pass's 5 M groups do not");
    let (one, two) = top_costs(&c);
    assert!(two < one, "the spilling one pass {one} prices above the two passes {two}");
    let (one0, two0) = top_costs(&TopCostIn { w0_spill_row: 0.0, ..c });
    assert!(one - one0 > 0.04 * c.rows && (two - two0).abs() < 1e-9, "the one pass alone pays the spill: {one0} -> {one}, {two0} -> {two}");
    let h015 = TopCostIn { rows: 1e8, groups: 1e8, groups_met: 1e8, keys: 1e8, cand: 30.0, cand_groups: 10.0, grant: 7680.0 * 1048576.0, group_bytes: group_record_bytes(2, 3), byref_state_bytes: 0.0, ..c };
    assert!(h015.one_pass_holds(), "the record alone holds");
    let (one_r, two_r) = top_costs(&h015);
    let (one_t, two_t) = top_costs(&TopCostIn { byref_state_bytes: 112.0, ..h015 });
    assert!(one_t - one_r > 0.04 * h015.rows && (two_t - two_r).abs() < 1e-9, "the text copies make the one pass spill: {one_r} -> {one_t}");
}

/// Measured on the D2078-size cells (W0: the one pass ~1.1-1.5x faster; W8: the two passes x0.81, OF-TG2-1's fit): at
/// width 0 the second read costs more than the fold it saves, and pass 1's 333 k keys outgrow 8 MB anyway; at width 8
/// the one pass folds every row and merges the participants' 83 k groups each where the two passes fold 120 candidate
/// rows, pass 1 holds, and the two passes win; a
/// pass 1 that outgrows the hash memory admits every key at run time, so the two passes never win then; heavy folds
/// (six transitions a row) pay for the second read even at width 0 once pass 1 holds.
/// `pgrust.gravity_grant_mb` is the execution's grant as a whole (exec room.rs `granted_memory`): pass 1's room whatever
/// the width or `work_mem` (an 8192 MB grant holds D2078's 15 MB pass 1 at W0; a 8 MB grant does not, even at W8).
#[test]
fn a_gravity_grant_is_pass_1s_room() {
    let w0 = d2078_cell();
    assert!(!w0.pass1_holds(), "W0, 8 MB of hash memory: pass 1 spills");
    assert!(TopCostIn { grant: (8192u64 << 20) as f64, ..w0 }.pass1_holds(), "the 8192 MB grant holds it");
    assert!(!TopCostIn { grant: (8u64 << 20) as f64, width: 8.0, ..w0 }.pass1_holds(), "an 8 MB grant does not, at any width");
}

#[test]
fn top_costs_take_two_passes_only_where_gravity_saves() {
    let w0 = d2078_cell();
    let (one, two) = top_costs(&w0);
    assert!(!w0.pass1_holds() && two > one, "W0, 8 MB: one pass ({one} vs {two})");
    let big = TopCostIn { hash_mem: 1e12, ..w0 };
    let (one, two) = top_costs(&big);
    assert!(big.pass1_holds() && two > one, "W0, pass 1 held: the second read costs more than the fold it saves ({one} vs {two})");
    let w8 = TopCostIn { width: 8.0, ..w0 };
    let (one, two) = top_costs(&w8);
    assert!(w8.pass1_holds() && two < one, "W8: two passes, measured x0.81 of the one pass ({one} vs {two})");
    let w8_small = TopCostIn { hash_mem: 128.0 * 1024.0, ..w8 };
    let (one, two) = top_costs(&w8_small);
    assert!(!w8_small.pass1_holds() && two > one, "W8, 128 kB: pass 1 spills, every key is admitted ({one} vs {two})");
    let heavy = TopCostIn { fold_row: 6.0 * 0.0025 + 2.0 * 0.0025 + 0.02, ..big };
    let (one, two) = top_costs(&heavy);
    assert!(two < one, "W0, heavy folds, pass 1 held: two passes ({one} vs {two})");
    // no key to drop (every row a candidate): never two passes, at any width
    for width in [0.0, 2.0, 8.0] {
        let all = TopCostIn { cand: 2e6, cand_groups: 666e3, width, ..big };
        let (one, two) = top_costs(&all);
        assert!(two > one, "width {width}: no row to drop ({one} vs {two})");
    }
}

/// SPILL-TP1: a count top over 100.7 M rows of unique keys (the spill rows' corpus, `scripts/costcal/spill-setup.sql`)
/// under a grant neither form's table fits. The fitted reading has pass 1 admit every key, so the two passes
/// never win; the measured price reads the exact set a spilled pass 1 still names plus pass 1's table at the
/// measured `top.c.spill_pass1_*` price, against the one pass plus its partial groups at `top.c.spill_one_*`, and takes
/// the two passes (measured on the corpus at W8: two passes 0.27-0.57 s, one pass 0.88-1.54 s from 2048 MiB down). Under
/// a grant both hold, neither form pays a spill.
#[test]
fn a_pass_1_past_its_room_is_priced_by_the_measured_spill() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rows = 100_663_296.0;
    let c = TopCostIn {
        rows,
        groups: rows,
        groups_met: rows,
        keys: rows,
        cand: 10.0,
        cand_groups: 10.0,
        n: 10.0,
        key_corr: None,
        pass1_record: pass1_record_bytes(false),
        group_bytes: group_record_bytes(2, 1),
        grant: (983u64 << 20) as f64,
        width: 8.0,
        ..d2078_cell()
    };
    let shape = TopShape { lead_keys: rows, lead_spread: Some(1.0), key_spread: Some(1.0) };
    assert!(!c.pass1_holds() && !c.one_table_holds(), "983 MiB holds neither table");
    assert!((c.pass1_spill_mib() - rows * 24.0 * 8.0 / 7.0 / 1048576.0).abs() < 1.0, "pass 1 spills its whole table: {}", c.pass1_spill_mib());
    let (one, two) = top_costs(&c);
    assert!(two > one, "fitted (D459): every key admitted, one pass ({one} vs {two})");
    let (one, two, measured) = top_costs_priced(&c, &shape);
    assert!(measured && two < one, "measured: the spilled two passes price under the spilled one pass ({one} vs {two})");
    let big = TopCostIn { grant: (8192u64 << 20) as f64, ..c };
    assert!(big.pass1_holds() && big.one_table_holds() && big.pass1_spill_mib().abs() < f64::EPSILON && big.one_spill_mib().abs() < f64::EPSILON, "8192 MiB holds both");
    let (one_h, two_h, measured_h) = top_costs_priced(&big, &shape);
    assert!(measured_h && one_h < one && two_h < two, "held, no spill: {one_h} < {one}, {two_h} < {two}");
}

#[test]
fn the_one_pass_merges_its_partials_in_the_owner_phase() {
    let big = TopCostIn {
        scan: 335_000.0,
        rows: 1e8,
        groups: 18e6,
        keys: 18e6,
        cand: two_pass_candidates(1e8, 18e6, 10.0),
        cand_groups: 18e6,
        n: 10.0,
        fold_row: 0.0025 + 2.0 * 0.0025,
        fold_group: 0.01,
        merge_row: 0.0025 + 2.0 * 0.0025,
        cpu_op: 0.0025,
        pass1_record: pass1_record_bytes(false),
        hash_mem: 1024.0 * 1024.0 * 1024.0 * 2.0,
        key_corr: None,
        grant: (8192u64 << 20) as f64,
        width: 8.0,
        group_bytes: group_record_bytes(2, 1),
        t1_record: phase1_record_bytes(2, 1, 0),
        fold_key: 2.0 * 0.0025,
        groups_met: 18e6,
        w0_spill_row: 0.0,
        byref_state_bytes: 0.0,
    };
    assert!(big.pass1_holds(), "an 8192 MB grant holds pass 1");
    let (one, two) = top_costs(&big);
    assert!(one < two, "W8: the one pass ({one} vs {two})");
    let (one, two) = top_costs(&TopCostIn { width: 0.0, ..big });
    assert!(one < two, "W0: the one pass ({one} vs {two})");
    let (one, two) = top_costs(&TopCostIn { width: 8.0, ..d2078_cell() });
    assert!(two < one, "D2078 W8: two passes ({one} vs {two})");
}

/// A held-out unique-key case (`SELECT i, tc, count(*) FROM c8 GROUP BY 1, 2 ORDER BY 3 DESC, 1 DESC, 2 DESC LIMIT 5
/// OFFSET 3`): 200 k groups of one row each, so no key repeats and pass 2 folds the 8 bound rows only, but pass 1 keeps a
/// key a row and ranks all 200 k of them for the bound, as many as the one pass's Sort ranks groups: the second read is
/// then the two passes' whole difference, whatever the read costs. Measured at W0: the two passes 45.3 ms, the one pass
/// ~24 (its cardinality twin 23.7-24.3). The ranking is a bounded heap's (`cost_sort`): a top of few keys pays little.
#[test]
fn a_top_of_one_row_groups_takes_the_one_pass_at_width_0() {
    let uniq = |scan: f64| TopCostIn {
        scan,
        rows: 200e3,
        groups: 200e3,
        keys: 200e3,
        cand: two_pass_candidates(200e3, 200e3, 8.0),
        cand_groups: 8.0,
        n: 8.0,
        fold_row: 0.0025 + 2.0 * 0.0025,
        fold_group: 0.01,
        merge_row: 0.0025 + 2.0 * 0.0025,
        cpu_op: 0.0025,
        pass1_record: pass1_record_bytes(false),
        hash_mem: 8.0 * 1024.0 * 1024.0,
        key_corr: None,
        grant: 0.0,
        width: 0.0,
        group_bytes: group_record_bytes(2, 1),
        t1_record: phase1_record_bytes(2, 1, 0),
        fold_key: 2.0 * 0.0025,
        groups_met: 200e3,
        w0_spill_row: 0.0,
        byref_state_bytes: 0.0,
    };
    for scan in [500.0, 4_500.0, 40_000.0] {
        let (one, two) = top_costs(&uniq(scan));
        assert!(one < two, "scan {scan}, W0: the one pass ({one} vs {two})");
    }
    let (_, eight) = top_costs(&uniq(4_500.0));
    let (_, one_key) = top_costs(&TopCostIn { n: 1.0, ..uniq(4_500.0) });
    assert!((eight - one_key - 2.0 * 0.0025 * 200e3 * 3.0).abs() < 1e-6, "{eight} - {one_key}");
    assert!(uniq(4_500.0).one_pass_holds());
}

#[test]
fn pass_1_ranks_for_free_above_width_0() {
    let s1 = TopCostIn { groups: 2e6, keys: 1.98e6, cand: two_pass_candidates(2e6, 1.98e6, 20.0), cand_groups: 20.0, fold_row: 0.0025 + 2.0 * 0.0025, pass1_record: pass1_record_bytes(false), key_corr: None, width: 4.0, ..d2078_cell() };
    let (_, two) = top_costs(&s1);
    let (_, two_n1) = top_costs(&TopCostIn { n: 1.0, ..s1 });
    assert_eq!(two, two_n1, "no ranking charge at W4");
    let w0 = TopCostIn { width: 0.0, ..s1 };
    let ((_, two0), (_, two0_n1)) = (top_costs(&w0), top_costs(&TopCostIn { n: 1.0, ..w0 }));
    assert!((two0 - two0_n1 - 2.0 * 0.0025 * 1.98e6 * (40f64.log2() - 1.0)).abs() < 1e-6, "W0 ranks: {two0} - {two0_n1}");
}

#[test]
fn pass1_records_are_the_key_the_count_and_the_extreme() {
    assert_eq!(pass1_record_bytes(false), 24.0, "key, count, validity");
    assert_eq!(pass1_record_bytes(true), 32.0, "a nullable key's NULL flag word");
    assert_eq!(pass1_extreme_record_bytes(false, false, false), 32.0, "key, count, extreme, validity");
    assert_eq!(pass1_extreme_record_bytes(false, true, false), 40.0, "a nullable column's non-NULL count");
    assert_eq!(pass1_extreme_record_bytes(true, true, false), 48.0, "a nullable key's NULL flag word");
    assert_eq!(pass1_extreme_record_bytes(false, true, true), 48.0, "GT-1: a spread top's best word");
}

#[test]
fn spread_candidates_grow_with_the_rows_a_key() {
    assert!((spread_candidates(1e6, 1e6, 10.0) - extreme_candidates(1e6, 1e6, 10.0)).abs() < 1e-6);
    let (few, many) = (spread_candidates(1e6, 5e5, 10.0), spread_candidates(1e6, 1e3, 10.0));
    assert!(few < 1e4 && few >= 10.0, "two rows a key admit under 1 % of the rows: {few}");
    assert!(many > 0.99e6, "a thousand rows a key admit nearly all: {many}");
    assert_eq!(spread_candidates(0.0, 5.0, 10.0), 0.0);
    assert_eq!(spread_candidates(1e6, 5.0, 10.0), 1e6);
}

#[test]
fn an_extreme_pass_1_is_read_per_participant() {
    let cell = |keys: f64, corr: f64, width: f64| TopCostIn { keys, width, key_corr: Some(corr), pass1_record: pass1_extreme_record_bytes(false, false, false), ..d2078_cell() };
    for width in [4.0, 8.0] {
        assert!(cell(150e3, 0.0, width).pass1_holds(), "W{width}: 150 k scattered keys hold");
        assert!(!cell(200e3, 0.0, width).pass1_holds(), "W{width}: 200 k scattered keys spill");
        assert!(cell(180e3, 0.0, width).pass1_holds() && !cell(185e3, 0.0, width).pass1_holds(), "W{width}: the traced line, 3/4 of 243 k pool records");
        assert!(!cell(500e3, 0.0, width).pass1_holds(), "W{width}: 500 k scattered keys spill");
        assert!(cell(500e3, 1.0, width).pass1_holds(), "W{width}: 500 k clustered keys hold");
    }
    assert!(cell(1e6, 1.0, 8.0).pass1_holds() && !cell(1e6, 1.0, 4.0).pass1_holds(), "1 M clustered keys: W8 holds, W4 spills");
    assert!(!cell(2e6, 1.0, 8.0).pass1_holds(), "2 M clustered keys spill at W8");
    let h = cell(200e3, 0.004, 8.0);
    let (one, two) = top_costs(&TopCostIn { groups: 400e3, ..h });
    assert!(!h.pass1_holds() && one < two, "D2078 h W8: the one pass ({one} vs {two})");
    // OF-TG2-1: at W2 each participant's 166 k of g's 333 k clustered keys reach a 40-byte record's 200 k pool once and
    // never fill it (past the 150 k a fold keeps: the fold never runs); at W0 the leader's 333 k fill it and spill
    for width in [2.0, 4.0, 8.0] {
        assert!(TopCostIn { width, ..d2078_cell() }.pass1_holds(), "D2078 g W{width}: pass 1 holds");
    }
    assert!(!TopCostIn { width: 0.0, ..d2078_cell() }.pass1_holds(), "D2078 g W0: pass 1 spills");
    // a scattered key whose participant's rows never fill its pool holds past the fold's 182 k: 400 k scattered keys of
    // 1.6 M rows at W8 are 200 k records a participant, under 243 k
    assert!(TopCostIn { rows: 1.6e6, ..cell(400e3, 0.0, 8.0) }.pass1_holds() && !cell(400e3, 0.0, 8.0).pass1_holds());
    assert!(TopCostIn { grant: (8192u64 << 20) as f64, ..cell(2e6, 0.0, 8.0) }.pass1_holds());
    assert!(!TopCostIn { grant: (8u64 << 20) as f64, ..cell(150e3, 0.0, 8.0) }.pass1_holds());
}

#[rustfmt::skip]
const OF_TG2_1_FIT: &[(&str, f64, [f64; 15], Option<f64>, f64, f64)] = &[
    ("s1", 2.0, [36658.0, 2000000.0, 1184343.0, 1184343.0, 815677.0, 815677.0, 20.0, 0.0075, 0.01, 0.0075, 24.0, 32.0, 0.005, 2000000.0, 8388608.0], None, 77.71, 51.99),
    ("s1", 4.0, [36658.0, 2000000.0, 1184343.0, 1184343.0, 815677.0, 815677.0, 20.0, 0.0075, 0.01, 0.0075, 24.0, 32.0, 0.005, 2000000.0, 8388608.0], None, 15.36, 24.94),
    ("s1", 8.0, [36658.0, 2000000.0, 1184343.0, 1184343.0, 815677.0, 815677.0, 20.0, 0.0075, 0.01, 0.0075, 24.0, 32.0, 0.005, 2000000.0, 8388608.0], None, 8.7, 8.91),
    ("s3", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 61.74, 71.96),
    ("s3", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 27.8, 33.84),
    ("s3", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 17.06, 12.83),
    ("s4", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 31.54, 92.35),
    ("s4", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 16.57, 39.97),
    ("s4", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 10.02, 15.98),
    ("s5", 2.0, [36658.0, 2000000.0, 200000.0, 194486.0, 1805534.0, 200000.0, 20.0, 0.0075, 0.01, 0.0075, 24.0, 32.0, 0.005, 388972.0, 8388608.0], None, 83.47, 42.91),
    ("s5", 4.0, [36658.0, 2000000.0, 200000.0, 194486.0, 1805534.0, 200000.0, 20.0, 0.0075, 0.01, 0.0075, 24.0, 32.0, 0.005, 388972.0, 8388608.0], None, 67.57, 21.55),
    ("s6", 2.0, [36658.0, 2000000.0, 200000.0, 194486.0, 205.7, 205.7, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 388972.0, 8388608.0], Some(0.09620321), 147.04, 85.02),
    ("s6", 4.0, [36658.0, 2000000.0, 200000.0, 194486.0, 205.7, 205.7, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 388972.0, 8388608.0], Some(0.09620321), 74.93, 43.42),
    ("s8", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.02, 0.01, 0.02, 40.0, 96.0, 0.005, 661886.0, 8388608.0], Some(1.0), 89.41, 239.08),
    ("s8", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.02, 0.01, 0.02, 40.0, 96.0, 0.005, 661886.0, 8388608.0], Some(1.0), 34.79, 199.83),
    ("s8", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.02, 0.01, 0.02, 40.0, 96.0, 0.005, 661886.0, 8388608.0], Some(1.0), 21.02, 126.22),
    ("x1", 2.0, [16330.0, 1000000.0, 306002.0, 306002.0, 179915.5, 55034.5, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 74.51, 39.42),
    ("x1", 4.0, [16330.0, 1000000.0, 306002.0, 306002.0, 179915.5, 55034.5, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 41.5, 21.58),
    ("x1", 8.0, [16330.0, 1000000.0, 306002.0, 306002.0, 179915.5, 55034.5, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 9.18, 7.2),
    ("x2", 2.0, [16330.0, 1000000.0, 306002.0, 306002.0, 65.4, 65.4, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 77.42, 50.45),
    ("x2", 4.0, [16330.0, 1000000.0, 306002.0, 306002.0, 65.4, 65.4, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 39.3, 23.35),
    ("x2", 8.0, [16330.0, 1000000.0, 306002.0, 306002.0, 65.4, 65.4, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 612004.0, 8388608.0], Some(0.0005194336), 8.21, 7.98),
    ("vmaxv", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 62.37, 73.91),
    ("vmaxv", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 28.34, 34.67),
    ("vmaxv", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 17.59, 13.09),
    ("vmaxw", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 31.28, 75.21),
    ("vmaxw", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 16.68, 35.75),
    ("vmaxw", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 9.99, 13.5),
    ("vminv", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 33.28, 74.09),
    ("vminv", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 17.66, 34.8),
    ("vminv", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 40.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 10.69, 13.13),
    ("vminw", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 31.43, 75.1),
    ("vminw", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 16.79, 35.76),
    ("vminw", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.0075, 0.01, 0.0075, 32.0, 40.0, 0.005, 661886.0, 8388608.0], Some(1.0), 9.99, 13.5),
    ("vminws", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 32.18, 96.39),
    ("vminws", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 17.04, 40.84),
    ("vminws", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 120.9, 120.9, 20.0, 0.01, 0.01, 0.01, 32.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 10.18, 16.23),
    ("vmaxvs", 2.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.01, 0.01, 0.01, 40.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 64.17, 94.72),
    ("vmaxvs", 4.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.01, 0.01, 0.01, 40.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 29.6, 39.79),
    ("vmaxvs", 8.0, [36658.0, 2000000.0, 330943.0, 330943.0, 611941.2, 101238.8, 20.0, 0.01, 0.01, 0.01, 40.0, 48.0, 0.005, 661886.0, 8388608.0], Some(1.0), 18.27, 15.69),
    ("r60", 2.0, [38692.0, 2000000.0, 119708.0, 59854.0, 668.3, 668.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 119708.0, 8388608.0], Some(-0.002453786), 57.33, 53.59),
    ("r60", 4.0, [38692.0, 2000000.0, 119708.0, 59854.0, 668.3, 668.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 119708.0, 8388608.0], Some(-0.002453786), 37.64, 27.76),
    ("r60", 8.0, [38692.0, 2000000.0, 119708.0, 59854.0, 668.3, 668.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 119708.0, 8388608.0], Some(-0.002453786), 20.5, 15.24),
    ("r90", 2.0, [38692.0, 2000000.0, 180956.0, 90478.0, 442.1, 442.1, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 180956.0, 8388608.0], Some(0.0046925214), 57.73, 75.2),
    ("r90", 4.0, [38692.0, 2000000.0, 180956.0, 90478.0, 442.1, 442.1, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 180956.0, 8388608.0], Some(0.0046925214), 38.15, 39.93),
    ("r90", 8.0, [38692.0, 2000000.0, 180956.0, 90478.0, 442.1, 442.1, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 180956.0, 8388608.0], Some(0.0046925214), 20.92, 22.18),
    ("r120", 2.0, [38692.0, 2000000.0, 200000.0, 120603.0, 331.7, 331.7, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 241206.0, 8388608.0], Some(0.0038744295), 58.19, 74.71),
    ("r120", 4.0, [38692.0, 2000000.0, 200000.0, 120603.0, 331.7, 331.7, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 241206.0, 8388608.0], Some(0.0038744295), 38.66, 39.54),
    ("r150", 2.0, [38692.0, 2000000.0, 200000.0, 147407.0, 271.4, 271.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 294814.0, 8388608.0], Some(-0.0021500674), 58.6, 75.33),
    ("r150", 4.0, [38692.0, 2000000.0, 200000.0, 147407.0, 271.4, 271.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 294814.0, 8388608.0], Some(-0.0021500674), 30.09, 39.22),
    ("r200", 2.0, [38692.0, 2000000.0, 200000.0, 198603.0, 201.4, 201.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 397206.0, 8388608.0], Some(-0.0034828063), 148.4, 81.38),
    ("r200", 4.0, [38692.0, 2000000.0, 200000.0, 198603.0, 201.4, 201.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 397206.0, 8388608.0], Some(-0.0034828063), 75.98, 41.82),
    ("r200", 8.0, [38692.0, 2000000.0, 200000.0, 198603.0, 201.4, 201.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 397206.0, 8388608.0], Some(-0.0034828063), 41.51, 27.98),
    ("r300", 2.0, [38692.0, 2000000.0, 293420.0, 293420.0, 136.3, 136.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 586840.0, 8388608.0], Some(0.004732853), 155.49, 88.33),
    ("r300", 4.0, [38692.0, 2000000.0, 293420.0, 293420.0, 136.3, 136.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 586840.0, 8388608.0], Some(0.004732853), 83.08, 46.27),
    ("r300", 8.0, [38692.0, 2000000.0, 293420.0, 293420.0, 136.3, 136.3, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 586840.0, 8388608.0], Some(0.004732853), 44.72, 27.26),
    ("r500", 2.0, [38692.0, 2000000.0, 526650.0, 526650.0, 76.0, 76.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1053300.0, 8388608.0], Some(0.0074284957), 152.63, 86.33),
    ("r500", 4.0, [38692.0, 2000000.0, 526650.0, 526650.0, 76.0, 76.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1053300.0, 8388608.0], Some(0.0074284957), 82.27, 44.53),
    ("r500", 8.0, [38692.0, 2000000.0, 526650.0, 526650.0, 76.0, 76.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1053300.0, 8388608.0], Some(0.0074284957), 48.78, 26.95),
    ("c500", 2.0, [38692.0, 2000000.0, 496802.0, 496802.0, 80.5, 80.5, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 993604.0, 8388608.0], Some(1.0), 112.75, 79.65),
    ("c500", 4.0, [38692.0, 2000000.0, 496802.0, 496802.0, 80.5, 80.5, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 993604.0, 8388608.0], Some(1.0), 17.23, 45.63),
    ("c500", 8.0, [38692.0, 2000000.0, 496802.0, 496802.0, 80.5, 80.5, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 993604.0, 8388608.0], Some(1.0), 10.24, 14.11),
    ("c1000", 2.0, [38692.0, 2000000.0, 991122.0, 991122.0, 40.4, 40.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1982244.0, 8388608.0], Some(1.0), 138.27, 94.31),
    ("c1000", 4.0, [38692.0, 2000000.0, 991122.0, 991122.0, 40.4, 40.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1982244.0, 8388608.0], Some(1.0), 73.55, 49.04),
    ("c1000", 8.0, [38692.0, 2000000.0, 991122.0, 991122.0, 40.4, 40.4, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 1982244.0, 8388608.0], Some(1.0), 12.0, 30.15),
    ("c2000", 2.0, [38692.0, 2000000.0, 2000000.0, 2000000.0, 20.0, 20.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 2000000.0, 8388608.0], Some(1.0), 168.21, 93.74),
    ("c2000", 4.0, [38692.0, 2000000.0, 2000000.0, 2000000.0, 20.0, 20.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 2000000.0, 8388608.0], Some(1.0), 91.44, 48.78),
    ("c2000", 8.0, [38692.0, 2000000.0, 2000000.0, 2000000.0, 20.0, 20.0, 20.0, 0.0075, 0.01, 0.0075, 32.0, 32.0, 0.005, 2000000.0, 8388608.0], Some(1.0), 53.69, 30.1),
];

/// OF-TG2-1: above width 0 `top_costs` takes the measured faster form on every pair of the fit table, outside a 5 % tie
/// and the fit's two misses: the extreme tops by the fitted weights, the count tops (s1, s5: `GROUP BY k, j` / `h, j`
/// with `count(*)`, a 2-key 1-aggregate group record) by PL-07's reading. The misses keep the one pass where the two
/// passes ran faster (the fit set's r90 at W2 / W8), as trunk does.
#[test]
fn top_costs_take_the_measured_form_above_width_0() {
    let misses = [("r90", 2.0), ("r90", 8.0)];
    for &(name, width, v, key_corr, two_ms, one_ms) in OF_TG2_1_FIT {
        let [scan, rows, groups, keys, cand, cand_groups, n, fold_row, fold_group, merge_row, pass1_record, t1_record, fold_key, groups_met, hash_mem] = v;
        let group_bytes = group_record_bytes(2, 1);
        let c = TopCostIn { scan, rows, groups, keys, cand, cand_groups, n, fold_row, fold_group, merge_row, cpu_op: 0.0025, pass1_record, hash_mem, key_corr, grant: 0.0, width, group_bytes, t1_record, fold_key, groups_met, w0_spill_row: 0.0, byref_state_bytes: 0.0 };
        let (one, two) = top_costs(&c);
        let tie = (two_ms / one_ms).ln().abs() <= 0.05;
        if misses.contains(&(name, width)) {
            assert!(one < two && two_ms < one_ms, "{name} W{width}: a known miss (the one pass)");
            continue;
        }
        assert!(tie || (two < one) == (two_ms < one_ms), "{name} W{width}: priced two / one x{:.3}, measured x{:.3}", two / one, two_ms / one_ms);
    }
}

#[test]
fn the_one_pass_spills_past_its_fold_room() {
    let g = TopCostIn { groups: 330_943.0, groups_met: 661_886.0, ..d2078_cell() };
    assert!(!TopCostIn { width: 4.0, ..g }.group_table_holds() && TopCostIn { width: 8.0, ..g }.group_table_holds());
    assert!(!TopCostIn { width: 8.0, t1_record: phase1_record_bytes(2, 6, 3), ..g }.group_table_holds());
    let x = TopCostIn { rows: 1e6, groups: 306_002.0, groups_met: 612_004.0, key_corr: Some(0.0005), ..d2078_cell() };
    assert!(!TopCostIn { width: 4.0, ..x }.group_table_holds() && TopCostIn { width: 8.0, ..x }.group_table_holds());
    assert!(TopCostIn { width: 8.0, t1_record: phase1_record_bytes(2, 2, 1), ..x }.group_table_holds(), "x2's 48-byte record: 113 k met, 124 k kept");
}

#[test]
fn a_held_scattered_one_pass_builds_no_two_passes() {
    let r60 = |width: f64| TopCostIn { scan: 38_692.0, groups: 119_708.0, keys: 59_854.0, cand: 668.3, cand_groups: 668.3, pass1_record: 32.0, t1_record: 32.0, key_corr: Some(-0.0025), width, groups_met: 119_708.0, ..d2078_cell() };
    for width in [2.0, 4.0, 8.0] {
        assert!(r60(width).scattered_one_pass_holds(), "r60 W{width}");
    }
    assert!(!r60(0.0).scattered_one_pass_holds(), "width 0 keeps its reading");
    let r120 = |width: f64| TopCostIn { groups: 200_000.0, keys: 120_603.0, groups_met: 241_206.0, ..r60(width) };
    assert!(!r120(2.0).scattered_one_pass_holds() && !r120(4.0).scattered_one_pass_holds(), "r120 W2 / W4: the one pass spills");
    let x2 = TopCostIn { rows: 1e6, groups: 306_002.0, keys: 306_002.0, groups_met: 612_004.0, key_corr: Some(0.0005), t1_record: phase1_record_bytes(2, 2, 1), width: 8.0, ..d2078_cell() };
    assert!(x2.scattered_one_pass_holds(), "x2 W8");
    assert!(!TopCostIn { width: 8.0, ..d2078_cell() }.scattered_one_pass_holds(), "a clustered key is priced");
}

#[test]
fn a_count_top_keeps_pl07s_reading_above_width_0() {
    let c1000 = TopCostIn { rows: 2e6, groups: 1e6, keys: 1e6, cand: two_pass_candidates(2e6, 1e6, 20.0), cand_groups: 1e6, fold_row: 0.0025 + 2.0 * 0.0025, pass1_record: pass1_record_bytes(false), key_corr: None, width: 4.0, groups_met: 2e6, ..d2078_cell() };
    let (one, two) = top_costs(&c1000);
    assert!(one < two, "W4: the one pass ({one} vs {two})");
}

#[test]
fn count_candidates_read_the_keys_seen_once() {
    let ks = |ndv: f64, mcv_keys: f64, mcv_freq: f64| KeySample { ndv, rows: 2e6, mcv_keys, mcv_freq, sample: 30_000.0 };
    let near = |got: f64, want: f64| (got / want - 1.0).abs() < 0.01;
    let k = count_candidates(2e6, 20.0, ks(1_210_298.0, 35.0, 0.0081));
    assert!(near(k, 394_680.0) && k < two_pass_candidates(2e6, 1_210_298.0, 20.0), "D2078's k: {k}");
    let c1000 = count_candidates(2e6, 20.0, ks(980_241.0, 0.0, 0.0));
    assert!(near(c1000, 1_614_953.0) && c1000 > two_pass_candidates(2e6, 980_241.0, 20.0), "c1000: {c1000}");
    assert!(near(count_candidates(2e6, 20.0, ks(211_273.0, 10.0, 0.102)), 1_551_398.0));
    assert_eq!(count_candidates(2e6, 20.0, ks(2e6, 0.0, 0.0)), 20.0);
    assert_eq!(count_candidates(2e6, 20.0, ks(2.0, 2.0, 1.0)), 2e6);
    assert_eq!(count_candidates(2e6, 20.0, ks(20_015.0, 1.0, 0.00027)), 2e6);
    assert!(near(count_candidates(1e6, 20.0, ks(980_241.0, 0.0, 0.0)), 1_614_953.0 / 2.0 + 10.0));
    assert_eq!(count_candidates(0.0, 20.0, ks(980_241.0, 0.0, 0.0)), 0.0);
}

#[test]
fn a_count_top_prices_the_keys_seen_twice() {
    let top = |scan: f64, ndv: f64, cand: f64, width: f64| TopCostIn { scan, rows: 2e6, groups: ndv, keys: ndv, cand, cand_groups: ndv.min(cand).max(20.0), fold_row: 0.0025 + 2.0 * 0.0025, merge_row: 0.0025 + 2.0 * 0.0025, pass1_record: pass1_record_bytes(false), key_corr: None, width, t1_record: phase1_record_bytes(2, 1, 0), groups_met: 2.0 * ndv, ..d2078_cell() };
    let c1000 = count_candidates(2e6, 20.0, KeySample { ndv: 980_241.0, rows: 2e6, mcv_keys: 0.0, mcv_freq: 0.0, sample: 30_000.0 });
    let (one, two) = top_costs(&top(38_692.0, 980_241.0, two_pass_candidates(2e6, 980_241.0, 20.0), 8.0));
    assert!(two < one, "c1000 W8 read by the repeats: the two passes ({one} vs {two})");
    let (one, two) = top_costs(&top(38_692.0, 980_241.0, c1000, 8.0));
    assert!(one < two, "c1000 W8: the one pass ({one} vs {two})");
    let s1 = count_candidates(2e6, 20.0, KeySample { ndv: 1_210_298.0, rows: 2e6, mcv_keys: 35.0, mcv_freq: 0.0081, sample: 30_000.0 });
    let s1 = s1.max(two_pass_candidates(2e6, 1_210_298.0, 20.0));
    assert_eq!(s1, two_pass_candidates(2e6, 1_210_298.0, 20.0), "s1: the repeats' floor");
    let (one, two) = top_costs(&top(36_658.0, 1_210_298.0, s1, 4.0));
    assert!(two < one, "s1 W4: the two passes ({one} vs {two})");
}

fn agg_of<'m>(mcx: Mcx<'m>, fnoid: u32, aggtype: u32, expr: Node<'m>) -> Node<'m> {
    let mut aggargtypes = types_nodes::list::OidList::nil();
    aggargtypes.lappend(mcx, 23).unwrap();
    let args = NodeList::make1(mcx, tle(mcx, expr, 1, "arg", 0)).unwrap();
    Node::mk(mcx, Aggref { aggfnoid: fnoid, aggtype, aggargtypes, args, ..Aggref::default() }).unwrap()
}

fn ordered_by<'m>(mcx: Mcx<'m>, agg: Node<'m>, int8: bool, desc: bool) -> Query<'m> {
    let tlist: Vec<(Node<'m>, &'m str, u32)> = vec![(var(mcx, 1), "pk", 1), (plus_one(mcx, var(mcx, 2)), "val1", 2), (agg, "a", 3)];
    let (eq, op) = match (int8, desc) {
        (true, true) => (INT8EQ_OP, INT8GT_OP),
        (true, false) => (INT8EQ_OP, INT8_LT_OP),
        (false, true) => (INT4EQ_OP, INT4_GT_OP),
        (false, false) => (INT4EQ_OP, INT4_LT_OP),
    };
    let mut parse = query(mcx, None, &tlist, &[1, 2], Some((3, eq, op, desc)));
    limit(&mut parse, mcx, 10);
    parse
}

const MAX_INT4: u32 = 2116;
const MIN_INT4: u32 = 2132;
const SUM_INT4: u32 = 2108;
const INT8_LT_OP: u32 = 412;

#[test]
fn two_pass_takes_any_bounded_order_key() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _fitted = FittedTop::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![], SPILL_AT);
    let two = Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: false });
    let plan = plan_of(mcx, ordered_by(mcx, agg_of(mcx, MAX_INT4, 23, var(mcx, 2)), false, true), "… ORDER BY max(val) DESC LIMIT 10");
    assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Sort, NodeTag::T_Agg, NodeTag::T_SeqScan]);
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: true, floor_nulls_first: true }), "max(x) DESC: the hint carries its order");
    let plan = plan_of(mcx, ordered_by(mcx, agg_of(mcx, MIN_INT4, 23, var(mcx, 2)), false, false), "… ORDER BY min(val) LIMIT 10");
    assert_eq!(agg_rewrite(&plan), two, "min(x) ASC");
    for (agg, desc, what) in [(MAX_INT4, false, "max(x) ASC"), (MIN_INT4, true, "min(x) DESC")] {
        let plan = plan_of(mcx, ordered_by(mcx, agg_of(mcx, agg, 23, var(mcx, 2)), false, desc), what);
        assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: desc, floor_nulls_first: desc }), "{what}: GT-1's spread rule, the hint carrying its order");
    }
    let plan = plan_of(mcx, ordered_by(mcx, agg_of(mcx, SUM_INT4, 20, var(mcx, 2)), true, true), "sum(x) DESC");
    assert_eq!(agg_rewrite(&plan), None, "sum(x) DESC");
    assert_eq!(*LAST_DECLINE.lock().unwrap_or_else(|e| e.into_inner()), Some("two-pass: the first sort key is not count(*) DESC or a min / max / integer avg of a word column"), "sum(x) DESC");
    let plan = plan_of(mcx, ordered_by(mcx, agg_of(mcx, MAX_INT4, 23, plus_one(mcx, var(mcx, 2))), false, true), "max(val + 1) DESC");
    assert_eq!(agg_rewrite(&plan), None, "an extreme over an expression");
    // ORDER BY count(*) DESC, pk, val + 1: the key after k1 never orders two keys seen once
    let mut parse = two_pass_query(mcx, count_star(mcx), None, Some((INT4_LT_OP, false)));
    parse.sortClause.lappend(mcx, sgc(mcx, 2, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
    let plan = plan_of(mcx, parse, "… ORDER BY c DESC, pk, val + 1 LIMIT 10");
    assert_eq!(agg_rewrite(&plan), two, "a count top with a key past k1");
}

#[test]
fn extreme_candidates_are_the_n_first_keys_rows() {
    assert_eq!(extreme_candidates(1e6, 1e6, 10.0), 10.0);
    assert_eq!(extreme_candidates(1e6, 1e3, 10.0), 1e4);
    assert_eq!(extreme_candidates(1e6, 5.0, 10.0), 1e6);
    assert_eq!(extreme_candidates(0.0, 5.0, 10.0), 0.0);
}

#[test]
fn two_pass_is_built_over_a_heap_scan() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _fitted = FittedTop::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![], SPILL_AT);
    *TEST_FACTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let plan = plan_of(mcx, two_pass_query(mcx, count_star(mcx), None, None), "heap: SELECT pk, val + 1, count(*) c FROM t GROUP BY pk, val + 1 ORDER BY c DESC LIMIT 10");
    assert_eq!(tags(&plan), [NodeTag::T_Limit, NodeTag::T_Sort, NodeTag::T_Agg, NodeTag::T_SeqScan]);
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: false, floor_nulls_first: false }));
}



/// the gate on the qual-class order: a LIKE whose constant pattern ends in its escape
/// raises (PostgreSQL's order stands), and so does one under a collation it cannot vouch for; a quiet pattern, a regex
/// constant and a numeric comparison (non-leakproof in the catalog, raise-free) do not.
#[test]
fn quiet_qual_classifies_patterns_and_numeric_comparisons() {
    let cx = setup();
    let mcx = cx.mcx();
    let s = || Node::mk_var(mcx, 1, 2, 25, -1, 100, 0).unwrap();
    let op = |f: u32, coll: u32, a, b| {
        let mut args = NodeList::make1(mcx, a).unwrap();
        args.lappend(mcx, b).unwrap();
        Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: 0, opfuncid: f, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: coll, args, location: -1 }).unwrap()
    };
    let pat = |p: &str| Node::mk_const(mcx, 25, -1, 100, -1, super::text_datum(mcx, p), false, false).unwrap();
    let q = |n| crate::createplan::quiet_qual(n, &|_| true).unwrap();
    assert!(!q(op(850, 100, s(), pat("\\"))), "LIKE '\\' raises 22025");
    assert!(!q(op(851, 100, s(), pat("r%\\"))), "NOT LIKE 'r%\\' too");
    assert!(q(op(850, 100, s(), pat("%atlas%"))), "a quiet LIKE");
    assert!(q(op(1633, 100, s(), pat("a\\\\"))), "an escaped escape is quiet");
    assert!(!q(op(850, 12345, s(), pat("%g%"))), "a collation it cannot vouch for");
    assert!(q(op(1254, 100, s(), pat("^r2"))), "a regex constant");
    let n = || Node::mk_var(mcx, 1, 3, 1700, -1, 0, 0).unwrap();
    assert!(q(op(1721, 0, n(), n())), "numeric_ge");
    let c = |attno, coll| Node::mk_var(mcx, 1, attno, 25, -1, coll, 0).unwrap();
    assert!(!q(op(67, 0, c(2, 950), c(4, 951))), "texteq under an indeterminate collation raises");
    assert!(q(op(67, 950, c(2, 950), c(4, 951))), "texteq under COLLATE \"C\"");
    let aq = |n| crate::agg_algebra::quiet(n).unwrap();
    assert!(!aq(op(67, 0, c(2, 950), c(4, 951))) && aq(op(67, 950, c(2, 950), c(4, 951))));
}

/// an InitPlan's PARAM_EXEC is no quiet leaf (its first read runs the InitPlan: 21000, nextval), so a qual class
/// never runs `t = (SELECT ..)` ahead of a qual PostgreSQL runs first; an extern parameter and a PARAM_EXEC the caller
/// vouches for (a correlation parameter) stay quiet.
#[test]
fn quiet_qual_reads_an_initplan_param_as_a_raise() {
    use types_nodes::primnodes::{OpExpr, Param, ParamKind};
    let cx = setup();
    let mcx = cx.mcx();
    let t = || Node::mk_var(mcx, 1, 2, 25, -1, 100, 0).unwrap();
    let prm = |paramkind, paramid| Node::mk(mcx, Param { paramkind, paramid, paramtype: 25, paramtypmod: -1, paramcollid: 100, location: -1 }).unwrap();
    let eq = |b| {
        let mut args = NodeList::make1(mcx, t()).unwrap();
        args.lappend(mcx, b).unwrap();
        Node::mk(mcx, OpExpr { opno: 98, opfuncid: 67, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 100, args, location: -1 }).unwrap()
    };
    let initplan = |id: i32| id == 0;
    let q = |n| crate::createplan::quiet_qual(n, &|id| !initplan(id)).unwrap();
    assert!(!q(eq(prm(ParamKind::PARAM_EXEC, 0))), "t = (InitPlan 1).col1");
    assert!(!q(prm(ParamKind::PARAM_EXEC, 0)), "a bare EXISTS InitPlan output");
    assert!(q(eq(prm(ParamKind::PARAM_EXEC, 3))), "a correlation parameter");
    assert!(q(eq(prm(ParamKind::PARAM_EXTERN, 1))), "$1");
}

/// `initplan_plan_quiet`, the proof that an InitPlan runs unseen ahead of the
/// row PostgreSQL first reads its parameter on: raise-free, non-volatile, and under an EXPR sublink at most one row.
#[test]
fn initplan_plan_quiet_proves_a_raise_free_row() {
    use types_nodes::plannodes::{Agg, Limit, Plan, Result as ResultPlan, Scan, SeqScan};
    use types_nodes::primnodes::{CoerceViaIO, CoercionForm, Param, ParamKind};
    let cx = setup();
    let mcx = cx.mcx();
    let list = |n| NodeList::make1(mcx, n).unwrap();
    let plan = |e, lefttree| Plan { targetlist: list(tle(mcx, e, 1, "x", 0)), lefttree, ..Plan::default() };
    let scan = || Node::mk(mcx, SeqScan { scan: Scan { plan: plan(var(mcx, 2), None), scanrelid: 1 }, ..SeqScan::default() }).unwrap();
    let agg = |a, ncols| Node::mk(mcx, Agg { plan: plan(a, Some(scan())), numCols: ncols, ..Agg::default() }).unwrap();
    let q = |p, one_row| crate::createplan::initplan_plan_quiet(p, one_row, &|id| id != 5).unwrap();
    let max = || agg_of(mcx, 2116, 23, var(mcx, 2));
    assert!(q(agg(max(), 0), true), "(SELECT max(k) FROM t): one row");
    assert!(!q(agg(agg_of(mcx, 2108, 20, var(mcx, 2)), 0), true), "sum(int4) is off the list");
    assert!(!q(agg(max(), 1), true), "GROUP BY: a second row raises 21000");
    assert!(q(agg(max(), 0), false) && !q(agg(max(), 1), false), "under EXISTS the rows are free, a grouped Agg is read loud");
    assert!(!q(scan(), true) && q(scan(), false), "a scan's rows");
    let int8 = |n| Node::mk_const(mcx, 20, -1, 0, 8, Datum::from_i64(n), false, true).unwrap();
    let limit = |n| Node::mk(mcx, Limit { plan: Plan { lefttree: Some(scan()), ..Plan::default() }, limitCount: Some(int8(n)), ..Limit::default() }).unwrap();
    assert!(q(limit(1), true) && !q(limit(2), true), "LIMIT 1");
    let k = Node::mk_var(mcx, 1, 4, 25, -1, 100, 0).unwrap();
    let cast = Node::mk(mcx, CoerceViaIO { arg: k, resulttype: 23, resultcollid: 0, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap();
    assert!(!q(agg(agg_of(mcx, 2116, 23, cast), 0), true), "max(k::int4) raises 22P02");
    let res = |e| Node::mk(mcx, ResultPlan { plan: plan(e, None), resconstantqual: None }).unwrap();
    let prm = |paramid| Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid, paramtype: 23, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
    assert!(q(res(int8(7)), true), "(SELECT 7)");
    assert!(q(res(prm(4)), true) && !q(res(prm(5)), true), "a nested InitPlan's parameter, by exec_quiet");
}

#[test]
fn initplan_plan_quiet_reads_the_comparison_quiet_extrema_and_count_distinct() {
    use types_nodes::plannodes::{Agg, Plan, Scan, SeqScan};
    let cx = setup();
    let mcx = cx.mcx();
    let list = |n| NodeList::make1(mcx, n).unwrap();
    let plan = |e, lefttree| Plan { targetlist: list(tle(mcx, e, 1, "x", 0)), lefttree, ..Plan::default() };
    let scan = || Node::mk(mcx, SeqScan { scan: Scan { plan: plan(var(mcx, 2), None), scanrelid: 1 }, ..SeqScan::default() }).unwrap();
    let q = |a| crate::createplan::initplan_plan_quiet(Node::mk(mcx, Agg { plan: plan(a, Some(scan())), ..Agg::default() }).unwrap(), true, &|_| true).unwrap();
    let typed = |t: u32, coll: u32| Node::mk_var(mcx, 1, 2, t, -1, coll, 0).unwrap();
    let call = |f: u32, t: u32, coll: u32, distinct: bool| {
        let args = NodeList::make1(mcx, tle(mcx, typed(t, coll), 1, "arg", u32::from(distinct))).unwrap();
        let aggdistinct = if distinct { NodeList::make1(mcx, sgc(mcx, 1, 0, 0, false)).unwrap() } else { NodeList::nil() };
        Node::mk(mcx, Aggref { aggfnoid: f, aggtype: if f == 2147 { 20 } else { t }, inputcollid: coll, args, aggdistinct, ..Aggref::default() }).unwrap()
    };
    for (f, t) in [(2797, 27), (2798, 27), (4189, 3220), (4190, 3220), (5099, 5069), (5100, 5069), (2128, 1186), (2144, 1186), (2124, 1266), (2140, 1266), (2116, 23), (2146, 1700), (2122, 1082)] {
        assert!(q(call(f, t, 0, false)), "aggregate {f} over type {t}");
    }
    for (f, t) in [(3526, 3500), (2244, 1042), (3564, 869), (2050, 2277), (2108, 23), (2101, 23)] {
        assert!(!q(call(f, t, 0, false)), "aggregate {f} over type {t}");
    }
    assert!(q(call(2129, 25, 100, false)) && !q(call(2129, 25, 12345, false)), "max(text): under the default collation only");
    assert!(q(call(2147, 23, 0, true)) && q(call(2147, 1186, 0, true)) && q(call(2147, 1700, 0, true)), "count(DISTINCT int4 / interval / numeric)");
    assert!(!q(call(2147, 25, 100, true)) && !q(call(2147, 1042, 100, true)), "count(DISTINCT text / bpchar): a collation, stays lazy");
    assert!(!q(call(2116, 23, 0, true)), "max(DISTINCT int4): DISTINCT only for count");
}

#[test]
fn two_pass_keys_pass_1_by_a_non_leading_group_key() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _fitted = FittedTop::hold();
    let cx = setup();
    let mcx = cx.mcx();
    let _l = Latch::hold(vec![], SPILL_AT);
    let tlist: Vec<(Node<'_>, &str, u32)> = vec![(plus_one(mcx, var(mcx, 2)), "val1", 1), (var(mcx, 1), "pk", 2), (agg_of(mcx, MAX_INT4, 23, var(mcx, 2)), "a", 3)];
    let mut parse = query(mcx, None, &tlist, &[1, 2], Some((3, INT4EQ_OP, INT4_GT_OP, true)));
    limit(&mut parse, mcx, 10);
    let plan = plan_of(mcx, parse, "GROUP BY val + 1, pk ORDER BY max(val) DESC LIMIT 10");
    assert_eq!(agg_rewrite(&plan), Some(GravityRewrite::TwoPass { attno: 1, floor_desc: true, floor_nulls_first: true }), "pass 1 keyed by pk, the second group key");
}
