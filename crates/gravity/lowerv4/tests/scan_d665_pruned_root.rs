//! D665: a DELETE through a partitioned root pruned to one leaf (part-028: `DELETE FROM part WHERE k = 5 AND s = 'x'`,
//! `resultRelations` the one leaf, `rootRelation` the root) freezes a plan that names the root (`WritePlan::with_route`,
//! no leaves), so the write host locks the root and fires the root's statement triggers (ExecInitModifyTable's
//! rootRelation: fireBSTriggers / fireASTriggers on it, at zero rows too). Before the fix the plan named only the leaf:
//! served through write.transition, the root's BEFORE / AFTER STATEMENT triggers never fired (e2e m4_d665_stmt_zero,
//! m4_write_route rt_r). G4: a DELETE of a plain table takes no route.
use gravity_lowerv4_it as common;

use common::RigCatalog;
use gravity_decompose::corpus;
use gravity_program::Arena;
use mcx::MemoryContext;

/// The fixture's lowering as the server's admission runs it: (target relid, the frozen route as (root, leaf count)), or
/// the refusal's text.
fn lower(text: &str) -> Result<(u32, Option<(u32, usize)>), String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let (ctx, arena) = (MemoryContext::new("d665 fixture"), Arena::new(1 << 20));
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map_err(|e| format!("decompose: {e:?}"))?;
    gravity_expr::nodes::func::acl_defer_begin();
    let l = gravity_lowerv4::lower_graph(arena.handle(), rig.mcx(), &graph, &fx.facts, RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    let l = l.map_err(|e| e.what.to_owned())?;
    let w = &l.descs.write;
    Ok((w.target_rel, w.route.as_ref().map(|r| (r.root, r.leaves.len()))))
}

fn plan(id: &str) -> String {
    std::fs::read_to_string(common::plans_dir().join(format!("{id}.plan"))).unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// part-028 with its root's facts changed: statement triggers on DELETE (the rig catalog's trigger facts).
fn with_root_stmt_triggers(id: &str) -> String {
    let from = "-- trigext 16395 ";
    let t = plan(id);
    assert!(t.lines().any(|l| l.starts_with(from)), "{id}: no root trigext line");
    t.lines()
        .map(|l| if l.starts_with(from) { "-- trigext 16395 before=- after=- instead=- stmt=d trans=- internal=- defer=- rules=f".to_owned() } else { l.to_owned() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The pruned DELETE's plan names its partitioned root with no leaves (the host's `under_root`), with and without
/// statement triggers on the root; the target stays the leaf.
#[test]
fn a_pruned_delete_names_its_partitioned_root() {
    assert_eq!(lower(&plan("part-028")), Ok((16399, Some((16395, 0)))));
    assert_eq!(lower(&with_root_stmt_triggers("part-028")), Ok((16399, Some((16395, 0)))));
}
