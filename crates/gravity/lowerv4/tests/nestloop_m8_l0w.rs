//! M8 L0-w (V4R-344; M8-plan §1.3a, main's ruling (3)): every executor parameter a producer of the graph sets gets its
//! word before any maker compiles (src/params.rs `supply`, called in lib.rs `lower_in` before the diag / serve split), and
//! its readers refuse by the producer's kind while that kind is unbuilt: step/consts.rs for an expression's need, the
//! index / bitmap / tid / sample sources and step/limit.rs for a key or bound. Before it no word was ever supplied, so
//! every such need folded and refused as "a parameter past the word budget" or "... without a parameter word". An
//! InitPlan's parameter places `Place::Exec` (its value is written at run time), never a bind word (main 05:27Z). Every
//! setParam of an InitPlan is its (FACTS-1a: a two-column ROWCOMPARE's, a MULTIEXPR UPDATE's second column), so none
//! refuses as a parameter no producer sets. RT-2 (main 09-27): a worker's frame pair is zeroed per section, so an InitPlan
//! value the leader set reads NOT SET on a worker; RT-1 runs Init programs at width 1 (safe today), and RT-2's worker
//! side must pass the leader's value in or run the Init once under the once-lock, never twice.
use gravity_lowerv4_it as common;

use gravity_expr::emit::ParamNeed;
use gravity_lower::{Place, Sequential, WordPlan, WordSrc};
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::frame::diag;
use gravity_lowerv4::{Reason, Variant};

/// The texts the parameter-word family refused by before L0-w (and the Shape a producer-less parameter refuses by now).
const OLD: &[&str] = &["a parameter past the word budget", "without a parameter word", "a LIMIT / OFFSET over a value the binder cannot place"];
const UNSET: &str = "an executor parameter no nest, SubPlan or InitPlan of the graph sets";

struct Lowered {
    blockers: Vec<diag::Blocker>,
    lines: Vec<String>,
    /// `(id, place of an Exec need of it after the lowering, its producer's kind)` for each id asked
    places: Vec<(i32, Place, Option<KindId>)>,
    exec_words: usize,
}

/// `rel` lowered in the all-blockers mode (tests/m5_l0_srf_args.rs's harness) over a `Sequential` plan, then each of
/// `ids` asked of the same plan as an expression's need.
fn lower(rel: &str, ids: &[i32]) -> Lowered {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    let p = common::plans_dir().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let (ctx, arena) = (mcx::MemoryContext::new("m8 l0w fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("{rel}: decompose {e:?}"));
    let mut plan = Sequential::new(arena.handle());
    gravity_expr::nodes::func::acl_defer_begin();
    let (_, report) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, common::RigCatalog::get(), &mut plan, gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let gx = gravity_lowerv4::graph::Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
    let exec_words = plan.words.iter().filter(|w| matches!(w, WordSrc::Exec { .. })).count();
    let places = ids.iter().map(|&id| (id, plan.need(&ParamNeed::Exec { id, typ: 23 }, 23, true), gravity_lowerv4::rerun::params::producer(&gx, id))).collect();
    Lowered { blockers: report.blockers, lines: report.lines, places, exec_words }
}

/// A maker's (not admission's) refusal by kind `k`'s name.
fn maker_refuses(l: &Lowered, k: KindId) -> bool {
    l.blockers.iter().any(|b| b.pipe.is_some() && b.refusal.reason == Variant::Kind(k).not_built())
}

fn check(rel: &str, ids: &[(i32, KindId)], reader: KindId) {
    let l = placed(rel, ids);
    assert!(maker_refuses(&l, reader), "{rel}: a reader refuses by {reader:?}: {:?}", l.lines);
}

/// `rel`'s parameters `ids` supplied by their producers' kinds as frame parameters, with no parameter-word Shape.
fn placed(rel: &str, ids: &[(i32, KindId)]) -> Lowered {
    let l = lower(rel, &ids.iter().map(|x| x.0).collect::<Vec<_>>());
    for (id, place, producer) in l.places.iter() {
        let want = ids.iter().find(|x| x.0 == *id).map(|x| x.1);
        assert_eq!(*producer, want, "{rel}: $exec{id}'s producer");
        assert_eq!(*place, Place::Exec(*id), "{rel}: $exec{id} places as the frame's parameter word, never a bind word");
    }
    assert_eq!(l.exec_words, 0, "{rel}: no WordSrc::Exec bind word is pushed");
    let old: Vec<&String> = l.lines.iter().filter(|x| OLD.iter().chain([&UNSET]).any(|t| x.contains(t))).collect();
    assert!(old.is_empty(), "{rel}: no parameter-word Shape: {old:?}");
    l
}

/// cte-008 `SELECT a FROM t WHERE a < 3 AND (SELECT count(*) FROM u) > 10`: the InitPlan's $exec0 read in a filter.
/// nest.initplan is built (M8-SUB B1), so the reader loads it (params.rs `InitLoad`) and nothing refuses.
#[test]
fn an_initplan_param_in_a_filter_serves_once_nest_initplan_is_built() {
    let l = lower("cte-008.plan", &[0]);
    assert_eq!(l.places.iter().map(|x| x.2).collect::<Vec<_>>(), vec![Some(KindId::NestInitplan)], "$exec0's producer");
    assert_eq!(l.exec_words, 0, "no WordSrc::Exec bind word is pushed");
    assert!(l.blockers.is_empty(), "{:?}", l.lines);
}

/// oltp-043 `... ol_o_id BETWEEN 1 AND (SELECT 3000 FROM pg_sleep(5))`: the InitPlan's $exec0 as an index key. Served
/// once nest.initplan is built (M8-SUB B1): pg_sleep's store is inside the InitPlan, and no call of it reads the parameter
/// (L0-l).
#[test]
fn an_initplan_param_as_an_index_key_serves_once_nest_initplan_is_built() {
    let l = placed("oltp-043.plan", &[(0, KindId::NestInitplan)]);
    assert!(l.blockers.is_empty(), "{:?}", l.lines);
}

/// cte-038 `UPDATE u SET (y, z) = (SELECT b, c FROM t WHERE t.a = u.x)`: the MULTIEXPR SubPlan's parParam $exec0 and its
/// setParams $exec1 / $exec2, the latter read by the write's projection. Served: the MULTIEXPR SubPlan sets its
/// setParams once per row (subplan item 13), so nothing refuses.
#[test]
fn a_multiexpr_setparam_serves() {
    let l = placed("cte-038.plan", &[(0, KindId::NestSubplan), (1, KindId::NestSubplan), (2, KindId::NestSubplan)]);
    assert!(l.blockers.is_empty(), "cte-038: {:?}", l.lines);
}

/// tpch-020 (TH20): the correlated SubPlan's $exec0 / $exec1 as index keys of its rerun pipeline p1; the nests' $exec2 /
/// $exec3 inside their embedded inners.
#[test]
fn a_subplan_param_in_a_rerun_pipeline_refuses_by_name() {
    // the parameters are placed by their producers; the rerun set is named by D1262's Shape where a maker reaches it (the
    // plan's nest inners refuse first under B1), never by the registered nest.rerun's NotBuilt name (main 20:29Z)
    let l = placed("tpch-020.plan", &[(0, KindId::NestSubplan), (1, KindId::NestSubplan), (2, KindId::NestParam), (3, KindId::NestParam)]);
    assert!(!l.lines.iter().any(|x| x.contains("NotBuilt M8: nest nest.rerun")), "tpch-020: names no built variant: {:?}", l.lines);
}

/// part-023 `u JOIN part ON part.k = u.y`: the nest's $exec0 as the index key of each partition's rerun pipeline. B1 built
/// nest.param, so its readers take the word, and nothing names nest.param. The inner is the partitions' union, which
/// PostgreSQL prunes at run time: S2b (rerun/inner_arms.rs) streams such a union only when every arm is quiet. Its arms are
/// one-index bitmap heap scans keyed by $exec0 under a builtin operator, quiet since landing 10 (D2015), so it is served
/// (was D2013 until then, D1483 until S2b, and D1018 for the rerun set's inner).
#[test]
fn a_nest_param_in_a_rerun_pipeline_is_served_over_quiet_bitmap_leaves() {
    let l = lower("part-023.plan", &[0]);
    assert_eq!(l.places, vec![(0, Place::Exec(0), Some(KindId::NestParam))], "part-023: $exec0 is the nest's, in its frame word");
    assert!(l.blockers.is_empty(), "part-023: {:?}", l.lines);
}

/// regress-full limit-010 `SELECT (SELECT n FROM (VALUES (1)) AS x, (SELECT n FROM generate_series(1,10) AS n ORDER BY n
/// LIMIT 1 OFFSET s-1) AS y) AS z FROM generate_series(1,10) AS s`: a top-k's OFFSET over the SubPlan's $exec0
/// (runs/topk_rows.rs, the census's limit#51: "a LIMIT / OFFSET over a value the binder cannot place" before). Since
/// C-R6f the set's top-k run is a member (rerun/sets.rs `members_close`); since the rerun group's top-N emit the nest
/// inner reads that run at the owner's region as it reads a member full sort's (rerun/sets.rs `emit_ok`, D1339 Row B),
/// its OFFSET read per open (D1634), so nothing refuses (was D1339's `D_RERUN_EMIT`).
#[test]
fn a_top_k_bound_over_a_subplan_param_is_served() {
    let l = placed("regress-full/limit-010.plan", &[(0, KindId::NestSubplan)]);
    assert!(l.blockers.is_empty(), "limit-010: a nest inner over its rerun set's top-k run is served: {:?}", l.lines);
}

/// FACTS-1a (main 06:50Z): an InitPlan's later setParams have their producer too, so no reader refuses by "no producer",
/// nor by nest.initplan once it is built (M8-SUB B1; a MULTIEXPR write's own walls are D652 / D1033): cte-041 `(a, b) = (SELECT x, y ...)` and regress-full subselect-018 / -019 (two-column ROWCOMPARE),
/// cte-042 `UPDATE u SET (y, z) = (SELECT max(b), min(c) FROM t)` and regress-full update-007 / -009 / -010 (MULTIEXPR
/// UPDATE), update-012 (a MULTIEXPR ON CONFLICT DO UPDATE).
#[test]
fn every_setparam_of_an_initplan_has_its_producer() {
    let rels = ["cte-041", "regress-full/subselect-018", "regress-full/subselect-019", "cte-042", "regress-full/update-007", "regress-full/update-009", "regress-full/update-010", "regress-full/update-012"];
    for rel in rels.map(|r| format!("{r}.plan")) {
        let l = placed(&rel, &[(0, KindId::NestInitplan), (1, KindId::NestInitplan)]);
        assert!(!maker_refuses(&l, KindId::NestInitplan), "{rel}: {:?}", l.lines);
    }
}

/// `params::outside` over each FROM function op's calls of `rel` (the calls a store evaluates in the ScanStore's
/// executor state, srf/store_read.rs `calls`), by pipeline.
fn outside(rel: &str) -> Vec<(usize, Option<KindId>)> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    let text = std::fs::read_to_string(common::plans_dir().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let (ctx, arena) = (mcx::MemoryContext::new("m8 l0l fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("{rel}: decompose {e:?}"));
    let gx = gravity_lowerv4::graph::Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut out = Vec::new();
    for (k, p) in graph.pipelines.iter().enumerate() {
        for op in p.ops.iter() {
            if let gravity_decompose::Op::Srf(d) = op {
                let calls: Vec<_> = d.functions.iter().map(|f| f.node).collect();
                out.push((k, gravity_lowerv4::rerun::params::outside(&gx, k, &calls)));
            }
        }
    }
    out
}

/// M8 L0-l (main 07:13Z): a store's calls run in the ScanStore's executor state, which shares no PARAM_EXEC, so a store
/// refuses by nest.initplan only when a call argument reads an InitPlan's parameter, never for an InitPlan elsewhere in
/// the graph (store_read.rs refused whenever the graph had one: once nest.initplan is built, admission no longer stops
/// first and oltp-043 would refuse by a built kind). oltp-043's `pg_sleep(5)` inside its own InitPlan reads none;
/// regress-full tsearch-019 has a store whose call reads the InitPlan's $exec5.
#[test]
fn a_store_waits_on_nest_initplan_only_when_a_call_reads_its_parameter() {
    let o = outside("oltp-043.plan");
    assert!(!o.is_empty() && o.iter().all(|x| x.1.is_none()), "oltp-043: pg_sleep's call reads no parameter: {o:?}");
    let t = outside("regress-full/tsearch-019.plan");
    assert!(t.iter().any(|x| x.1 == Some(KindId::NestInitplan)), "tsearch-019: a call reads the InitPlan's $exec5: {t:?}");
}

/// A parameter no producer of the graph sets folds as before (no word, no `Place::Exec`), and its producer is `None`.
#[test]
fn a_param_no_producer_sets_stays_unsupplied() {
    let l = lower("cte-008.plan", &[7]);
    assert_eq!(l.places, vec![(7, Place::Fold, None)]);
    assert!(l.blockers.iter().all(|b| !matches!(b.refusal.reason, Reason::Bug { .. })), "{:?}", l.lines);
}

/// V4R-361 (main 08:15Z): the Pipes view (src/pipes.rs) leaves every walk over the graph pipelines as it was. On every plan
/// fixture (decompose's corpus and each milestone's) `len`, `iter` and `[k]` / `get(k)` below `len` are the graph's own
/// pipelines, and only the indices past them reach the embedded inners, each the one its `(pipeline, op)` embeds.
#[test]
fn the_pipes_view_keeps_every_graph_walk_on_every_fixture() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
    use gravity_decompose::Op;
    common::RigCatalog::install_seams();
    let (mut graphs, mut inners, mut bad) = (0usize, 0usize, Vec::new());
    for dir in std::iter::once(common::plans_dir()).chain((1..=8).map(common::milestone_dir)) {
        for (name, text) in common::files(&dir, "plan") {
            let (ctx, arena) = (mcx::MemoryContext::new("m8 pipes view"), gravity_program::Arena::new(1 << 20));
            let Ok(fx) = gravity_decompose::corpus::load(ctx.mcx(), &text) else { continue };
            let Ok(graph) = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
            let gx = gravity_lowerv4::graph::Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
            let (v, g) = (gx.pipelines(), &graph.pipelines[..]);
            graphs += 1;
            let own = |k: usize| core::ptr::eq(&v[k], &g[k]) && v.get(k).is_some_and(|p| core::ptr::eq(p, &g[k]));
            if v.len() != g.len() || !v.iter().zip(g.iter()).all(|(a, b)| core::ptr::eq(a, b)) || !(0..g.len()).all(own) {
                bad.push(format!("{name}: a walk over 0..len() sees other than the graph's pipelines"));
            }
            let all = v.inners();
            for (i, &((k, j), p)) in all.iter().enumerate() {
                let embeds = match &v[k].ops[j] {
                    Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => core::ptr::eq(&**inner, p),
                    _ => false,
                };
                if !embeds || v.inner_of(k, j) != Some(g.len() + i) || !core::ptr::eq(&v[g.len() + i], p) {
                    bad.push(format!("{name}: inner {i} (pipeline {k}, op {j}) is not the one its op embeds"));
                }
            }
            if v.get(g.len() + all.len()).is_some() || v.end() != g.len() + all.len() {
                bad.push(format!("{name}: an index past the inners reaches a pipeline, or end() is not past the last inner"));
            }
            inners += all.len();
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert!(graphs > 800 && inners > 100, "{graphs} graphs, {inners} inners: the corpus did not load");
    eprintln!("pipes view: {graphs} graphs, {inners} embedded inners");
}

/// Row 2 (the inner.rs rerun hook, M8-NEST): tidrangescan-008 `LATERAL (SELECT count(*) FROM t WHERE ctid >= o.c ...)`:
/// the nest's inner is a plain aggregate's one emit over a rerun set member, read as the owner's first consumed region,
/// so the set re-runs at every re-open and nothing refuses. aggregates-018's LATERAL aggregate is a rerun set's emit
/// that reads the running pipeline's regions and resume word (D1339): Row C gives the embedded reader its own emit
/// state (keep-pins, the gather's own position), so it serves too, by neither D1339's Shape nor D1018's.
#[test]
fn a_plain_aggregate_rerun_inner_and_a_rerun_set_emit_both_serve() {
    for rel in ["regress-full/tidrangescan-008.plan", "regress-full/aggregates-018.plan"] {
        let l = lower(rel, &[]);
        assert!(l.blockers.is_empty() && !l.lines.iter().any(|x| x.contains("(D1339)") || x.contains("(D1018)")), "{rel}: {:?}", l.lines);
    }
}

/// D1710 (M8-WRITE): M1's DELETE end numbered its write site at its make, before an InitPlan's pipeline numbered its
/// source site, so the positional host registry found no recipe for it at run time; it refused by D652's name (fail
/// closed) until modifytable's D1710 change, which numbers the write site late whenever the statement has an InitPlan.
/// cte-040 with the InitPlan's value as the index key itself (`DELETE FROM u WHERE x = (SELECT min(a) FROM t)`, the
/// isolation specs' form) lowers. So do a DELETE end with no InitPlan (oltp-033), and an UPDATE beside an InitPlan
/// (cte-039: the row op as a step numbers its site late).
#[test]
fn a_delete_end_beside_an_initplan_numbers_its_site_late() {
    let text = std::fs::read_to_string(common::plans_dir().join("cte-040.plan")).expect("fixture");
    let key = "{OPEXPR :opno 551 :opfuncid 177 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({PARAM :paramkind 1 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1} {CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location 49 :constvalue 4 [ 160 134 1 0 0 0 0 0 ]}) :location 47}";
    assert_eq!(text.matches(key).count(), 2, "cte-040's index key, in indexqual and indexqualorig");
    let d1710 = text.replace(key, "{PARAM :paramkind 1 :paramid 1 :paramtype 23 :paramtypmod -1 :paramcollid 0 :location -1}").replace(") + 100000;", ");");
    if let Err(e) = common::lower_fixture(&d1710).expect("decomposes") {
        panic!("D1710's form refuses: {e}");
    }
    for rel in ["oltp-033.plan", "cte-039.plan"] {
        let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("fixture");
        if let Err(e) = common::lower_fixture(&text).expect("decomposes") {
            panic!("{rel} does not apply, yet refuses: {e}");
        }
    }
}

/// M8-NEST S2d (V4R-563, D2015's form C): a union a nest inner embeds with an arm under a Result's one-time gate lowers
/// with no D2015: census join-196 (join#361, an Index Only Scan arm beside the gated Result: regress-full/join-104 here)
/// and join-199 / -200 (join#365 / #366: regress-full/join-107). Its Merge / Hash join forms, census join-205 / -206
/// (join#375 / #376: regress-full/join-111) lowers too, and foreign_data-027 (#399 / #405: regress-full/foreign_data-016)
/// holds no S2d no-widen line (only the rig's catalog gaps): D2194's name left the gated arm at item 14 part 4.
#[test]
fn a_gated_union_arm_in_a_nest_inner_serves_beside_a_hash_or_merge_join() {
    let named = |x: &String| x.contains("(D2015)") || x.contains("(S2d)") || x.contains("(D2194 / D2147 / D2201)");
    for rel in ["regress-full/join-104.plan", "regress-full/join-107.plan", "regress-full/join-111.plan"] {
        let l = lower(rel, &[]);
        assert!(l.blockers.is_empty() && !l.lines.iter().any(named), "{rel}: {:?}", l.lines);
    }
    let l = lower("regress-full/foreign_data-016.plan", &[]);
    assert!(!l.lines.iter().any(named), "foreign_data-016: {:?}", l.lines);
}
