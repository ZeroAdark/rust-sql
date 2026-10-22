//! Serve before strategies: admission (applies.rs `pick` / `admit`) never refuses an unbuilt strategy (the statement serves
//! on the strategy's straight form: `face/source.rs` `on()` is false for it), while an unbuilt kind still refuses
//! `NotBuilt` by name; `unbuilt_later` lists the applicable strategies a statement serves without (the exit report); the
//! all-blockers mode logs them as `gravity: unbuilt-strategy` lines, never as blockers.
use gravity_lowerv4_it as common;

use gravity_lowerv4::applies::{self, Prop, PropSet};
use gravity_lowerv4::face::kinds::{KindId, KindSet};
use gravity_lowerv4::face::strategy::{self, StrategySet, SEMIJOIN_MARKS, SEMIJOIN_RUNS};
use gravity_lowerv4::frame::diag;
use gravity_lowerv4::props::Props;
use gravity_lowerv4::admit::refusal::Variant;
use gravity_lowerv4::{ops as registry, Reason};

/// A graph needing only `ExprCore` (node 5) whose kinds are every kind and whose properties are all on: nearly every
/// strategy applies (less the narrowed, dropped and `gravity_road_off` ones).
fn wide(m: mcx::Mcx<'_>) -> Props<'_> {
    let props = PropSet(Prop::ALL.iter().fold(0, |s, &p| s | 1u64 << p as u32));
    Props { need: mcx::to_vec_in(&[(KindId::ExprCore, 5), (KindId::JoinSemi, 9)], m), kinds: KindSet(u128::MAX), props, max_rows: 1, pipelines: 1 }
}

/// Over a synthetic registry (`pick_in`): with only `ExprCore` built, admission names the unbuilt kind at its node and
/// never a strategy; with both kinds built, the graph is admitted whatever strategies are built.
#[test]
fn an_unbuilt_strategy_never_refuses_and_an_unbuilt_kind_does() {
    let cx = mcx::MemoryContext::new("test");
    let p = wide(cx.mcx());
    assert!(applies::applicable(&p).len() > 20, "strategies apply");
    assert_eq!(applies::pick_in(&p, KindSet::of(&[KindId::ExprCore])), Some((Variant::Kind(KindId::JoinSemi), Some(9))));
    assert_eq!(applies::pick_in(&p, KindSet::of(&[KindId::ExprCore, KindId::JoinSemi])), None);
}

/// The exit report is exactly the applicable strategies the registry does not provide, and admission over the registry
/// refuses none of them.
#[test]
fn the_exit_report_is_the_unbuilt_applicable_strategies() {
    let cx = mcx::MemoryContext::new("test");
    let p = wide(cx.mcx());
    let built = registry::built_strategies();
    let mut want = StrategySet::EMPTY;
    applies::applicable(&p).iter().filter(|&r| !built.has(r)).for_each(|r| want.add(r));
    assert_eq!(applies::unbuilt_later(&p), want);
    assert!(!matches!(applies::pick(&p), Some((Variant::Strategy(_), _))), "admission refused a strategy");
}

/// The all-blockers mode over the census plan of `SELECT t.a FROM t LEFT JOIN u ON t.a = u.x WHERE u.x IS NULL` (a Hash
/// Anti Join; decompose tests/plans/regress-042.plan): semijoin.marks / semijoin.runs apply (join.anti) and are unbuilt:
/// neither is a blocker (the exit report carries them). Both are planned (road_off.rs `PLANNED`): off by default, so the
/// report leaves them out until `+<id>` names them on. The line's form is fixed.
#[test]
fn the_diag_mode_logs_unbuilt_strategies_as_lines_not_blockers() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let text = std::fs::read_to_string(common::plans_dir().join("regress-042.plan")).expect("regress-042.plan");
    common::RigCatalog::install_seams();
    let (ctx, arena) = (mcx::MemoryContext::new("m7 l0a fixture"), gravity_program::Arena::new(1 << 20));
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    let gx = gravity_lowerv4::graph::Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
    let p = Props::of(&gx);
    assert!(p.kinds.has(KindId::JoinAnti), "an anti join");
    let report = applies::unbuilt_later(&p);
    assert!(!report.has(SEMIJOIN_MARKS) && !report.has(SEMIJOIN_RUNS), "planned roads, off by default: {report:?}");
    let mut named_on = strategy::off();
    [SEMIJOIN_MARKS, SEMIJOIN_RUNS].into_iter().for_each(|r| named_on.remove(r));
    strategy::set_off(named_on);
    let report = applies::unbuilt_later(&p);
    for r in [SEMIJOIN_MARKS, SEMIJOIN_RUNS] {
        assert_eq!(report.has(r), !registry::built_strategies().has(r), "{}", r.name());
    }
    gravity_expr::nodes::func::acl_defer_begin();
    let (_, rep) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, common::RigCatalog::get(), &mut gravity_lower::Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
    gravity_expr::nodes::func::acl_defer_end();
    for b in &rep.blockers {
        assert!(!matches!(b.refusal.reason, Reason::NotBuilt { variant: Variant::Strategy(_), .. }), "a strategy listed as a blocker: {:?}", rep.lines);
    }
    assert!(rep.lines.iter().all(|l| l.starts_with("gravity: blocker ")), "{:?}", rep.lines);
    assert_eq!(
        diag::unbuilt_strategy_line(SEMIJOIN_RUNS),
        "gravity: unbuilt-strategy strategy=semijoin.runs text=an applicable strategy with no mechanism; the statement takes its straight form"
    );
}
