//! M5 L0-c (2) (M5-plan §1.3, §9 Wave 1a row 1a-2): a set-returning call's arguments are the Srf step's, never the
//! enclosing expression's needs. gravity_expr `compile_srf` compiles no argument of a set-returning `FuncExpr` (its value
//! is the cell the Srf step fills), so step/consts.rs `needs_of` stops there as it stops at an `Aggref`. Walking into the
//! call listed its literals as needs the program never reads, and `place` refused Shape "an expression whose parameter
//! needs are not its literals": the M5 dry run's cascade on 1,041 statements behind srf.projectset, P1 included. The
//! lowering runs in the all-blockers mode (frame/diag.rs), which makes each maker past srf.projectset's placeholder, so the
//! ProjectSet's Project is lowered today although the kind refuses.
use gravity_lowerv4_it as common;

use gravity_lowerv4::frame::diag;
use gravity_lowerv4::Reason;

const WHY: &str = "an expression whose parameter needs are not its literals";

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `rel` lowered in the mode (as tests/diag.rs `lower_diag`): the refusal's text (or served) and every blocker's line.
fn blockers(rel: &str) -> (Result<(), String>, Vec<diag::Blocker>, Vec<String>) {
    common::RigCatalog::install_seams();
    let (ctx, arena) = (mcx::MemoryContext::new("m5 l0 fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text(rel)).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|e| panic!("{rel}: decompose {e:?}"));
    gravity_expr::nodes::func::acl_defer_begin();
    let (r, report) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, common::RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    (r.map(|_| ()).map_err(|e| e.to_string()), report.blockers, report.lines)
}

/// The blockers' Shapes (every stage, cascade or not).
fn shapes(b: &[diag::Blocker]) -> Vec<String> {
    b.iter().filter(|b| matches!(b.refusal.reason, Reason::Shape { .. })).map(|b| format!("{:?}", b.refusal)).collect()
}

/// P1 (bench-089, `SELECT count(*) FROM (SELECT generate_series(1, a % 3) FROM g) s`): no Shape past the placeholders.
/// With the walk descending into the call, the ProjectSet's Project listed `1` and `3` against a program that reads no
/// parameter. Once srf.projectset is built (M5-PSET) P1 lowers, or refuses only by an unbuilt road's name at admission.
#[test]
fn p1_reaches_the_srf_step_without_the_cascade_shape() {
    let (r, b, lines) = blockers("bench-089.plan");
    if let Err(err) = &r {
        assert!(!err.contains("srf.projectset"), "srf.projectset is built: {err}");
    }
    // the makers past the placeholders ran (the ProjectSet's Project among them) and refused nothing: unbuilt names only
    assert!(b.iter().all(|b| matches!(b.refusal.reason, Reason::NotBuilt { .. })), "{lines:?}");
    assert!(shapes(&b).is_empty(), "no Shape: {:?}", shapes(&b));
    assert!(!lines.iter().any(|l| l.contains(WHY)), "{lines:?}");
}

/// Two calls in one select list, and a call under an operator the Project above the ProjectSet applies (srf-005,
/// `generate_series(1, b) * 2`: the `2` is still the outer expression's need, matched against its program's).
#[test]
fn every_projectset_fixture_is_free_of_the_shape() {
    for rel in ["srf-002.plan", "srf-003.plan", "srf-004.plan", "srf-005.plan", "srf-008.plan", "srf-012.plan", "srf-013.plan"] {
        let (_, _, lines) = blockers(rel);
        assert!(!lines.iter().any(|l| l.contains(WHY)), "{rel}: {lines:?}");
    }
}

/// Does not apply (G4): a call that returns one value keeps its literal arguments as needs. fieldobs-044's
/// regexp_replace(source_url, '^[a-z]+://...', '\1') serves with its two literals placed as words.
#[test]
fn a_scalar_calls_literals_stay_its_needs() {
    let (r, b, lines) = blockers("fieldobs-044.plan");
    assert!(r.is_ok() && b.is_empty(), "fieldobs-044 serves: {r:?} {lines:?}");
}
