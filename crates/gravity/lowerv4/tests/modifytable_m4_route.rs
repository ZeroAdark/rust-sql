//! M4-ROUTE R1 (cards/M4.md L3 `write/route.rs`, road write.route): the routed INSERT over the census plan fixtures,
//! decomposed as the server does (`decompose_in`). The frozen plan names
//! the root and every leaf the fixture's `-- part` lines give, in their order; a leaf the route host cannot write, a leaf
//! whose triggers fire on INSERT and unknown leaves refuse by name; G4: a plain INSERT takes no route.
//! R2: a DELETE of several result relations streams into its Append's arms (V4R-214), serial in child order, every arm
//! writing through the one write site under the plan routed over every target. M4-UPDATE: an UPDATE of several result
//! relations is the same road, its write site carrying the SET cells alone (the host forms each target's row).
use gravity_lowerv4_it as common;

use common::RigCatalog;
use gravity_decompose::corpus;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::Op;
use gravity_program::{Arena, End, Parallel, Reason, RouteLeaf, SiteKind};
use mcx::MemoryContext;

/// The fixture's lowering as the server's admission runs it: (target relid, the frozen route as (root, leaves)), or the
/// refusal's text.
fn lower(text: &str) -> Result<(u32, Option<(u32, Vec<RouteLeaf>)>), String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let (ctx, arena) = (MemoryContext::new("m4_route fixture"), Arena::new(1 << 20));
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map_err(|e| format!("decompose: {e:?}"))?;
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.map_err(|e| e.what.to_owned())?;
    let w = &l.descs.write;
    Ok((w.target_rel, w.route.as_ref().map(|r| (r.root, r.leaves.to_vec()))))
}

fn plan(id: &str) -> String {
    std::fs::read_to_string(common::plans_dir().join(format!("{id}.plan"))).unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// `plan(id)` with each header line starting `from` replaced by `to` (a leaf's facts changed).
fn edit(id: &str, from: &str, to: &str) -> String {
    let t = plan(id);
    assert!(t.lines().any(|l| l.starts_with(from)), "{id}: no header line {from:?}");
    t.lines().map(|l| if l.starts_with(from) { to } else { l }).collect::<Vec<_>>().join("\n") + "\n"
}

fn leaf(relid: u32, natts: u16) -> RouteLeaf {
    RouteLeaf { relid, natts, nindexes: 1, before_row: false }
}

/// part-024 (`INSERT INTO part VALUES (5, 1, 'x')`, three range leaves) and facts-026 (`INSERT INTO fp VALUES (150, 1)`, two
/// leaves) serve: the write is the root's (its layout), the route the root and the leaves as read.
#[test]
fn a_routed_insert_freezes_its_root_and_leaves() {
    assert_eq!(lower(&plan("part-024")), Ok((16395, Some((16395, vec![leaf(16399, 3), leaf(16405, 3), leaf(16411, 3)])))));
    assert_eq!(lower(&plan("facts-026")), Ok((16424, Some((16424, vec![leaf(16427, 2), leaf(16430, 2)])))));
}

/// Unknown leaves (no `-- part` lines: a facts source that could not answer), a leaf with rules and a leaf that is not a
/// heap table refuse by name before any row.
#[test]
fn a_leaf_the_route_host_cannot_write_refuses_by_name() {
    let no_parts: String = plan("facts-026").lines().filter(|l| !l.starts_with("-- part ")).map(|l| format!("{l}\n")).collect();
    let got = lower(&no_parts).unwrap_err();
    assert!(got.contains("partition leaves the facts source does not know"), "{got}");
    let got = lower(&edit("facts-026", "-- part 16424 16430 ", "-- part 16424 16430 trig=f rules=t")).unwrap_err();
    assert!(got.contains("a partition with rules"), "{got}");
    let got = lower(&edit("facts-026", "-- rel 16430 ", "-- rel 16430 fp2 am=other kind=r natts=2")).unwrap_err();
    assert!(got.contains("a partition that is not a heap table"), "{got}");
    // a foreign leaf (removed with the Foreign Scan): exec's admission raises 0A000 on this reason (admit/removed.rs)
    let got = lower(&edit("facts-026", "-- rel 16430 ", "-- rel 16430 fp2 am=heap kind=f natts=2")).unwrap_err();
    assert!(got.contains(gravity_decompose::ops::modifytable::route_leaves::FOREIGN_LEAF), "{got}");
}

/// D624: a leaf index the write host does not maintain (no `IndexPlan`: partial, expression, INCLUDE, deferrable,
/// exclusion) refuses by name, never a routed row that skips it (the server's facts: hosts facts.rs `leaf_fact` over
/// index_maint.rs `unmaintainable`; RouteHost re-checks it at the leaf's first row).
#[test]
fn a_leaf_index_the_host_does_not_maintain_refuses_by_name() {
    let base = "-- idx 16435 rel=16430 keys=1 opts=0 unique=f am=btree partial=f deferrable=f excl=f primary=f nkeyatts=1";
    let forms = [
        ("partial", base.replace("partial=f", "partial=t")),
        ("expression", base.replace("keys=1 ", "keys=0 ")),
        ("INCLUDE", base.replace("keys=1 ", "keys=1,2 ")),
        ("deferrable", base.replace("unique=f", "unique=t").replace("deferrable=f", "deferrable=t")),
        ("exclusion", base.replace("excl=f", "excl=t")),
    ];
    for (what, line) in forms {
        let got = lower(&edit("facts-026", "-- idx 16435 ", &line)).unwrap_err();
        assert!(got.contains("a partition with an index the lowered write cannot maintain"), "{what}: {got}");
    }
}

/// A leaf trigger that fires on INSERT (BEFORE ROW, or AFTER ROW as a referencing partition's RI check) is write.transition's:
/// NotBuilt by its name, never a routed row that skips it. An UPDATE-only leaf trigger does not fire and the INSERT serves.
#[test]
fn a_firing_leaf_trigger_is_write_transitions() {
    let trig = |before: &str, after: &str| {
        let t = edit("facts-026", "-- part 16424 16427 ", "-- part 16424 16427 trig=t rules=f");
        let line = format!("-- trigext 16427 before={before} after={after} instead=- stmt=- trans=- internal=- defer=- rules=f");
        t.lines().map(|l| if l.starts_with("-- trigext 16427 ") { line.as_str() } else { l }).collect::<Vec<_>>().join("\n") + "\n"
    };
    for (before, after) in [("i", "-"), ("-", "iu")] {
        // M4-TRIG T1 built write.transition: a leaf whose triggers fire on INSERT takes the trigger plan and routes
        let got = lower(&trig(before, after));
        assert!(got.as_ref().is_ok_and(|(_, r)| r.is_some()), "{before}/{after}: {got:?}");
    }
    assert!(lower(&trig("u", "u")).is_ok_and(|(_, r)| r.is_some()));
}

/// G4: an INSERT into a plain table takes no route (oltpc-015: the insert core's plan, `route` unset).
#[test]
fn a_plain_insert_takes_no_route() {
    let (_, route) = lower(&plan("oltpc-015")).expect("oltpc-015 serves");
    assert_eq!(route, None);
}

/// `delete from iboolpart where a is null`: a DELETE through a partitioned root over its three leaves.
const PP037: &str = "regress-full/partition_prune-037";

/// `plan(id)` with trigger facts that fire nothing on `rels` (the capture recorded none: unknown facts are
/// write.transition's).
fn quiet(id: &str, rels: &[u32]) -> String {
    let lines: String = rels.iter().map(|r| format!("-- trigext {r} before=- after=- instead=- stmt=- trans=- internal=- defer=- rules=f\n")).collect();
    plan(id).replacen("\n-- rel ", &format!("\n{lines}-- rel "), 1)
}

/// V4R-214's pin: a DELETE of several result relations (partition_prune-037: three leaves, none pruned) decomposes as one
/// pipeline per Append arm, no breaker, each arm serial for the write (`Ineligible`, `Reason::Write`: the leader runs it,
/// never a worker), marked arm `i` of `n` at graph index `i` (child order: ModifyTable pulls the Append's children in order,
/// so arm `i`'s rows are all written before arm `i + 1` reads a row), ending in the write over its own target's rows.
#[test]
fn a_several_target_delete_streams_serial_arms_in_child_order() {
    for (id, n) in [(PP037, 3)] {
        RigCatalog::install_seams();
        let (ctx, arena) = (MemoryContext::new("m4_route arms"), Arena::new(1 << 20));
        let fx = corpus::load(ctx.mcx(), &plan(id)).unwrap_or_else(|e| panic!("{id}: {e}"));
        let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("{id}: {e:?}"));
        assert_eq!((g.pipelines.len(), g.breakers.len()), (n, 0), "{id}");
        for (k, p) in g.pipelines.iter().enumerate() {
            assert!(matches!(p.desc.parallel, Parallel::Ineligible { reason: Reason::Write, .. }) && p.desc.ends_in == End::Sink, "{id} p{k}: {:?}", p.desc.parallel);
            assert!(p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { i, n: m, .. }) if *i as usize == k && *m as usize == n)), "{id} p{k}: not arm {k} of {n}");
            let w = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }).expect("the write in every arm");
            assert_eq!(w.targets.len(), n, "{id} p{k}");
        }
    }
}

/// The lowered DELETE of several result relations: every target frozen in `targets` order under the root, one write site
/// numbered after every arm's scan site (the arms share it: `write/rowop.rs` `Arms`).
#[test]
fn a_several_target_delete_serves_through_one_write_site() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let (ctx, arena) = (MemoryContext::new("m4_route targets"), Arena::new(1 << 20));
    let fx = corpus::load(ctx.mcx(), &quiet(PP037, &[44941, 44944, 44947, 44950])).unwrap();
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.unwrap_or_else(|e| panic!("{PP037}: {}", e.what));
    let r = l.descs.write.route.as_ref().expect("routed over the targets");
    let targets: Vec<u32> = graph.pipelines[0].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w.targets.iter().map(|t| t.relid).collect()) } else { None }).unwrap();
    assert_eq!((r.root, r.leaves.iter().map(|x| x.relid).collect::<Vec<_>>()), (44941, targets));
    let writes: Vec<usize> = l.descs.sites.iter().enumerate().filter(|(_, s)| matches!(s.kind, SiteKind::Write { .. })).map(|(i, _)| i).collect();
    assert_eq!(writes, vec![l.descs.sites.len() - 1], "one write site, after the arms' scan sites");
}

/// `DELETE FROM INSERT_TBL` once its only child is dropped (relhassubclass stays set): PG plans the parent as its own one
/// child under rootRelation (decompose Member::Targets, one target). With Member::Targets bound (V4R-214) write.delete's
/// single-target branch serves it and freezes the plan with the root (WritePlan::with_route, no leaves), as D665 does for
/// a pruned partitioned root: the host locks the root and fires its statement triggers, never the leaf's (e2e
/// m4_route_targets: one child left, the parent and several children, every relation excluded).
const CON006: &str = "regress-full/constraints-006";

#[test]
fn a_delete_of_an_inheritance_trees_one_target_names_its_root() {
    let (target, route) = lower(&quiet(CON006, &[20675])).unwrap_or_else(|e| panic!("{CON006}: {e}"));
    assert_eq!((target, route.map(|(root, leaves)| (root, leaves.len()))), (20675, Some((20675, 0))));
}

/// `UPDATE b SET aa='zzz' WHERE aa='aaa'` (inherit-007): an UPDATE through an inheritance parent over its child.
const INH007: &str = "regress-full/inherit-007";
/// `update p1 set a1 = a1 + 1, f2 = upper(f2)` (alter_table-022): the child numbers the SET columns otherwise
/// (`updateColnosLists ((i 2 3) (i 4 2))`: its a1 is attribute 4, its f2 attribute 2).
const AT022: &str = "regress-full/alter_table-022";

/// The lowered UPDATE (write/update.rs through write.route's TargetsRoad) and its write sites' widths, or the refusal.
fn lower_update(text: &str) -> Result<(u32, Vec<u32>, Vec<u16>), String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let (ctx, arena) = (MemoryContext::new("m4_route update"), Arena::new(1 << 20));
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map_err(|e| format!("decompose: {e:?}"))?;
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.map_err(|e| e.what.to_owned())?;
    let r = l.descs.write.route.as_ref().expect("routed over the targets");
    let widths = l.descs.sites.iter().filter_map(|s| if let SiteKind::Write { natts } = s.kind { Some(natts) } else { None }).collect();
    Ok((r.root, r.leaves.iter().map(|x| x.relid).collect(), widths))
}

/// An UPDATE of several result relations serves: every target frozen under the named relation in `targets` order, and ONE
/// write site (the arms share it) whose cells are the SET entries alone, whatever each target's own numbering of them.
#[test]
fn a_several_target_update_serves_its_set_cells_through_one_write_site() {
    assert_eq!(lower_update(&quiet(INH007, &[22481, 22491])), Ok((22481, vec![22481, 22491], vec![1])));
    assert_eq!(lower_update(&quiet(AT022, &[41888, 41891])), Ok((41888, vec![41888, 41891], vec![2])));
}

/// The host places SET cell `i` at the target attribute its root column names: a target whose SET list is not the root's,
/// in the root's order, refuses by Shape (the fixture's child list reordered so it maps to (f2, a1)).
#[test]
fn a_target_whose_set_list_is_not_the_roots_refuses_by_shape() {
    let t = quiet(AT022, &[41888, 41891]);
    assert!(t.contains(":updateColnosLists ((i 2 3) (i 4 2))"), "alter_table-022 changed");
    let got = lower_update(&t.replace(":updateColnosLists ((i 2 3) (i 4 2))", ":updateColnosLists ((i 2 3) (i 2 4))")).unwrap_err();
    assert!(got.contains("a target whose SET columns are not the root's"), "{got}");
}

/// `update some_tab set f3 = 11 where f1 = 12 and f2 = 13` (inherit-011: the parent excluded by its NO INHERIT CHECK, one
/// child left), `update some_tab set a = a + 1 where false` (inherit-012: every relation excluded, the parent the dummy's
/// one target) and `UPDATE INSERT_TBL SET x = NULL WHERE x = 5` (constraints-009: the parent planned as its own child):
/// an inheritance tree's ONE result relation is the single-target UPDATE under the root (WritePlan::with_route, no
/// leaves), as write.delete's: the host locks the root and fires its statement triggers, never the leaf's.
#[test]
fn an_update_of_an_inheritance_trees_one_target_names_its_root() {
    for (id, rels, target, root) in [("regress-full/inherit-011", &[22514, 22518][..], 22518, 22514), ("regress-full/inherit-012", &[22524, 22527][..], 22524, 22524), ("regress-full/constraints-009", &[20675][..], 20675, 20675)] {
        let (t, route) = lower(&quiet(id, rels)).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!((t, route.map(|(r, leaves)| (r, leaves.len()))), (target, Some((root, 0))), "{id}");
    }
}
