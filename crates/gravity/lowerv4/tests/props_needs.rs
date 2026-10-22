//! Props needs accuracy (PROPS-FIX, V4R revision approved Michael 2026-09-24): claimed => served needs props.rs to name
//! the mechanism the lowering actually asks for.
//! - D386 aggs-019 `GROUP BY f4, f8`: a float key of a keyed group that returns it keys through the hosted arm
//!   (rep.hosted.hash, M6): its word image folds `-0` / NaN, so KEYED refused it by Shape while props claimed it.
//! - D495 aggs-042 / aggs-043 `SELECT f8 | f4, count(DISTINCT i4) ... GROUP BY f8 | f4`: a count(DISTINCT) group's own
//!   float key is returned, so it keys through the hosted arm like D386's; only the DISTINCT argument's key (resno 0) is
//!   never output and keeps its word image (aggs-044 `count(DISTINCT f8) ... GROUP BY k`).
//! - D387 stats_ext-027 `GROUP BY ctid, a, b`: a `tid` key is rep.hosted.hash's (M6).
//! - D392 a TID scan's quals are key words the bind evaluates (source/tid.rs), not expressions: `ctid = ANY (...)` names
//!   no expr.saop, so regress-091 / tidscan-001 need src.tid alone.
//! - D455 (D391) a settled union (decompose's settle_arms: Feed -> the arms' spool -> Union emit; the parent's ops do not
//!   copy into the arms, e.g. a top-k over the Append) names nest.spool (M8): source/append.rs make_arm / make_settled /
//!   make_feed refuse by that name. A streamed union's arms (D393; into a full sort's end, V4R-159) and a MergeAppend's
//!   feeds name no spool.
//! - D396 aggs-035: a packed numeric sum / avg past one mantissa word in a keyed end is class.num_varlena's (fold/class_of.rs
//!   L0-e hands D297's wide case there), not class.num_packed's.
//! - M4-L0 L0-j (props/write.rs): a row op's trigger need counts the commands it runs beyond its own. INSERT ... ON
//!   CONFLICT DO UPDATE fires UPDATE's triggers; a cross-partition UPDATE fires DELETE's and INSERT's, on every leaf under
//!   its root (leaves the facts source cannot list count as firing). A missed trigger is a WRONG, so each names
//!   write.transition.
//! - M4-L0 L0-l (props/props.rs): values_only means no source reads rows beyond Result / VALUES. An Append over Result
//!   arms (srf-009.1), a TID, sample, index-array or index-AM scan reads rows, so it is not values_only, and an INSERT
//!   over one is an INSERT ... SELECT.
//! - M4-L0 NextValueExpr (props/expr.rs): an identity column's default is a NextValueExpr (nextval_internal, VOLATILE),
//!   not a FuncExpr, so the call walk missed it. It names expr.volatile, in a VALUES row and in a projection alike.
use gravity_lowerv4_it as common;

use std::collections::BTreeSet;

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn kinds(rel: &str) -> BTreeSet<String> {
    common::props_fixture(&text(rel)).unwrap_or_else(|| panic!("{rel}: decompose declined")).0
}

/// The verdict with the relations read as pgrcolumnar (no heap source needed), `served` or the refusal text.
fn verdict_pgrc(rel: &str) -> String {
    let t = text(rel).replace(" am=heap ", " am=pgrcolumnar ");
    match common::lower_fixture(&t).unwrap_or_else(|| panic!("{rel}: decompose declined")) {
        Ok(_) => "served".to_owned(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn d386_float_group_key_keys_by_its_word_image() {
    // M6 L0-m (V4R-276): a returned float key keys by its word image; the record keeps one member's datum for the output
    let k = kinds("aggs-019.plan");
    assert!(k.contains("rep.word.hash") && !k.contains("rep.hosted.hash"), "aggs-019 GROUP BY f4, f8: {k:?}");
    let v = verdict_pgrc("aggs-019.plan");
    assert!(!v.contains("rep.hosted.hash") && !v.contains("its image is not its output datum"), "aggs-019 keeps its float keys' datums: {v}");
}

#[test]
fn d386_float_aggregate_under_an_integer_key_is_left_alone() {
    // G4: floats folded (sum(f4), avg(f8)) under an int key keep the word key arm
    let k = kinds("aggs-005.plan");
    assert!(k.contains("rep.word.hash") && !k.contains("rep.hosted.hash"), "aggs-005 GROUP BY k: {k:?}");
}

#[test]
fn d495_distinct_group_float_key_keys_by_its_word_image() {
    // M6 L0-m: a count(DISTINCT) group's returned float key keys by its word image too (its datum kept in its rep slot)
    for f in ["aggs-042.plan", "aggs-043.plan"] {
        let k = kinds(f);
        assert!(k.contains("rep.word.hash") && !k.contains("rep.hosted.hash"), "{f}: a count(DISTINCT) group's returned float key: {k:?}");
    }
}

#[test]
fn d495_distinct_argument_float_key_keeps_its_image() {
    // G4: count(DISTINCT f8) under an int key: the argument's key is never output
    let k = kinds("aggs-044.plan");
    assert!(!k.contains("rep.hosted.hash"), "aggs-044 SELECT k, count(DISTINCT f8) ... GROUP BY k: {k:?}");
}

#[test]
fn d387_group_by_ctid_names_hosted_hash() {
    let k = kinds("regress-full/stats_ext-027.plan");
    assert!(k.contains("rep.hosted.hash"), "stats_ext-027 GROUP BY ctid, a, b: {k:?}");
    assert!(k.contains("rep.word.hash"), "stats_ext-027's int keys keep the word arm: {k:?}");
}

#[test]
fn d455_settled_union_names_nest_spool() {
    for f in ["facts-017.plan", "gsunion-014.plan", "setops-006.plan"] {
        let k = kinds(f);
        assert!(k.contains("nest.spool") && k.contains("src.append"), "{f}: a settled union: {k:?}");
    }
}

#[test]
fn d455_streamed_and_merged_appends_are_left_alone() {
    // G4: a streamed union (arm markers, the parent's ops copied; D393's fold; aggs-029's hashed grouping sets, V4R-130)
    // and a MergeAppend's ordered feeds
    for f in ["aggs-018.plan", "aggs-029.plan", "part-030.plan"] {
        let k = kinds(f);
        assert!(!k.contains("nest.spool"), "{f}: {k:?}");
    }
}

#[test]
fn d392_tid_key_quals_are_not_expressions() {
    for f in ["regress-091.plan", "regress-full/tidscan-001.plan"] {
        let k = kinds(f);
        assert!(k.contains("src.tid") && !k.contains("expr.saop"), "{f}: ctid = ANY (...) is a key word: {k:?}");
    }
}

#[test]
fn d392_a_tid_scans_filter_is_still_compiled() {
    // G4: tidscan-006's filter `(id = 3 AND ctid IN (...)) OR (ctid = '(0,1)' AND id = 1)` runs as a qual
    let k = kinds("regress-full/tidscan-006.plan");
    assert!(k.contains("src.tid") && k.contains("step.filter") && k.contains("expr.saop"), "tidscan-006: {k:?}");
}

#[test]
fn v4r38_wide_packed_sum_in_a_hashed_end_is_a_num_window_tree() {
    // D396 named class.num_varlena here; V4R-38 L2 (V4R-58): a Hashed group's end folds the tree (fold/num_keyed.rs)
    let k = kinds("aggs-035.plan");
    assert!(k.contains("class.num_packed") && !k.contains("class.num_varlena"), "aggs-035 sum(CASE .. n2 * n2 ..) GROUP BY k: {k:?}");
    assert_eq!(verdict_pgrc("aggs-035.plan"), "served", "aggs-035");
}

#[test]
fn d396_narrow_sums_and_plain_trees_keep_num_packed() {
    // G4: aggs-037's keyed sums fit one word; aggs-036's wide sum is a plain end's num.window tree
    for f in ["aggs-037.plan", "aggs-036.plan"] {
        let k = kinds(f);
        assert!(k.contains("class.num_packed") && !k.contains("class.num_varlena"), "{f}: {k:?}");
        assert_eq!(verdict_pgrc(f), "served", "{f}");
    }
}

#[test]
fn v4r549_a_sorted_case_sum_with_its_dscale_word_is_class_num_packed() {
    // V4R-549 (D297's wide form): TPC-H Q8's sum(CASE .. THEN l_extendedprice * (1 - l_discount) ELSE 0 END) in a Sorted
    // group carries a D256 dscale word: it folds in class.num_packed, no longer class.num_varlena; a Sorted group's keyed
    // end folds num.window trees as a Hashed one's does (num_keyed.rs `forms`), so its sum(volume) beside it (no dscale
    // word) folds its tree in class.num_packed too, where its datum was class.num_wide's
    let k = kinds("tpch-008.plan");
    assert!(k.contains("class.num_packed") && !k.contains("class.num_wide") && !k.contains("class.num_varlena"), "tpch-008: {k:?}");
    let heap = match common::lower_fixture(&text("tpch-008.plan")).expect("tpch-008 decomposes") {
        Ok(_) => "served".to_owned(),
        Err(e) => e.to_string(),
    };
    assert_eq!(heap, "served", "tpch-008 (heap, GroupAggregate)");
}

/// `rel` with `lines` added to its header (before its first `-- rel` line) and `swap`'s text replaced.
fn with_header(rel: &str, lines: &str, swap: Option<(&str, &str)>) -> BTreeSet<String> {
    let t = text(rel);
    let at = t.find("\n-- rel ").unwrap_or_else(|| panic!("{rel}: no rel line")) + 1;
    let mut t = format!("{}{}{}", &t[..at], lines, &t[at..]);
    if let Some((from, to)) = swap {
        assert!(t.contains(from), "{rel}: no {from:?}");
        t = t.replace(from, to);
    }
    all_members(&t).unwrap_or_else(|| panic!("{rel}: decompose declined"))
}

/// The kinds props.rs reads off `text` decomposed (decompose_in): a cross-partition UPDATE is
/// not in the server's set until write.route binds it, and L0-j's rule must hold when it is.
fn all_members(text: &str) -> Option<BTreeSet<String>> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    use gravity_lowerv4::{graph::Gx, props::Props};
    common::RigCatalog::install_seams();
    let ctx = mcx::MemoryContext::new("v4 props fixture");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = gravity_program::Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    let p = Props::of(&Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() });
    Some(p.kinds.iter().map(|k| k.name().to_owned()).collect())
}

#[test]
fn l0j_on_conflict_do_update_fires_update_triggers() {
    // regress-063 `INSERT INTO u ... ON CONFLICT (x) DO UPDATE`: u (16406) has no trigger (G4), an AFTER UPDATE ROW one
    // (ExecOnConflictUpdate fires it), an AFTER DELETE ROW one (never fired by the INSERT)
    let rel = "regress-063.plan";
    assert!(!kinds(rel).contains("write.transition"), "{rel}: no trigger");
    let upd = with_header(rel, "", Some(("-- trigext 16406 before=- after=-", "-- trigext 16406 before=- after=u")));
    assert!(upd.contains("write.transition"), "{rel}: an UPDATE trigger under DO UPDATE: {upd:?}");
    let del = with_header(rel, "", Some(("-- trigext 16406 before=- after=-", "-- trigext 16406 before=- after=d")));
    assert!(!del.contains("write.transition"), "{rel}: a DELETE trigger: {del:?}");
}

#[test]
fn l0j_cross_partition_update_fires_the_leaves_delete_and_insert_triggers() {
    // foreign_key-060 `UPDATE pk SET a = 90 WHERE a = 30`: pk (37506) > pk1 (37512) > pk11 (37518), the row moves leaves
    let rel = "regress-full/foreign_key-060.plan";
    let none = "-- trigext 37506 before=- after=- instead=- stmt=- trans=- internal=- defer=- rules=f\n\
                -- trigext 37518 before=- after=- instead=- stmt=- trans=- internal=- defer=- rules=f\n";
    let parts = "-- part 37506 37512 trig=f rules=f\n-- part 37512 37518 trig=f rules=f\n";
    let k = with_header(rel, none, None);
    assert!(k.contains("write.transition"), "{rel}: leaves unknown: {k:?}");
    let k = with_header(rel, &format!("{none}{parts}"), None);
    assert!(!k.contains("write.transition"), "{rel}: no leaf trigger: {k:?}");
    let k = with_header(rel, &format!("{none}{parts}"), Some(("-- trigext 37518 before=- after=-", "-- trigext 37518 before=- after=d")));
    assert!(k.contains("write.transition"), "{rel}: the source leaf's DELETE trigger: {k:?}");
    let other = "-- part 37512 37530 trig=t rules=f\n-- trigext 37530 before=i after=- instead=- stmt=- trans=- internal=- defer=- rules=f\n";
    let k = with_header(rel, &format!("{none}{parts}{other}"), None);
    assert!(k.contains("write.transition"), "{rel}: another leaf's BEFORE INSERT trigger: {k:?}");
}

#[test]
fn l0l_an_append_over_result_arms_is_not_values_only() {
    // srf-009.1 `SELECT three(), a FROM t WHERE a < 3` (census srf-010's nested plan): src.append over src.result arms
    let rel = "srf-009.1.plan";
    let (k, p) = common::props_fixture(&text(rel)).unwrap_or_else(|| panic!("{rel}: decompose declined"));
    assert!(k.contains("src.append") && k.contains("src.result"), "{rel}: {k:?}");
    assert!(!p.contains("values_only"), "{rel}: an Append reads rows: {p:?}");
}

#[test]
fn nextvalueexpr_names_expr_volatile() {
    // identity-007 `INSERT INTO itest5 OVERRIDING SYSTEM VALUE VALUES (DEFAULT, 'dd'), (-4, 'ee')`: in a VALUES row
    // stats_ext-007 `INSERT INTO stats_ext_tbl (col) VALUES ('secret'), ..`: the identity default in the projection
    for rel in ["regress-full/identity-007.plan", "regress-full/stats_ext-007.plan"] {
        assert!(text(rel).contains("NEXTVALUEEXPR"), "{rel}: no NextValueExpr");
        assert!(kinds(rel).contains("expr.volatile"), "{rel}: {:?}", kinds(rel));
    }
}

#[test]
fn d771_a_sort_over_a_numeric_final_names_the_arm_it_orders_through() {
    // partition_aggregate-007 `SELECT a, sum(b), avg(b), count(*) FROM pagg_tab GROUP BY 1 HAVING avg(d) < 15 ORDER BY
    // 1, 2, 3`: the Sort over a Sorted group's finals is not the group's own (no runs.topk_groups, not form G), so avg(b)'s
    // numeric final orders through its cell's arm, rep.hosted.order's (a numeric does not order by its bytes); since
    // M6-REPS HO built that arm the statement is served
    let rel = "regress-full/partition_aggregate-007.plan";
    let k = kinds(rel);
    assert!(k.contains("rep.hosted.order") && !k.contains("rep.varlena.order"), "{rel}: {k:?}");
    let v = verdict_pgrc(rel);
    assert!(v == "served", "{rel} is served once rep.hosted.order is built: {v}");
}

#[test]
fn m10_window_keys_are_typed_by_the_childs_column() {
    // M10-WINDOW (V4R-C, props/ops.rs reopen): a WindowAgg key names a column of its input (the stored rows, the child's
    // target list), never the WindowAgg's own target list entry of that number, as graphprops.py's census mirror reads
    // the graph's `partition=` / `order=` images. bench-085 `PARTITION BY t2, c1` and tsrf-017 `PARTITION BY
    // generate_series(1,3)` named a hosted order key from the window's own output column; triggers-006's name key is a
    // varlena order key.
    for rel in ["bench-085.plan", "regress-full/tsrf-017.plan"] {
        let k = kinds(rel);
        assert!(!k.contains("rep.hosted.order"), "{rel}: {k:?}");
    }
    let k = kinds("regress-full/triggers-006.plan");
    assert!(k.contains("rep.varlena.order"), "triggers-006: {k:?}");
}
