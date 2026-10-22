//! D1657's once rule (M8-RERUN; split from srf/function_scan.rs for its budget, main 08:22Z): PG's FunctionScan fills
//! each function's tuplestore once and at a rescan recomputes only a function whose `funcparams` a changed parameter
//! meets (ExecReScanFunctionScan), else rewinds it. `reopen_once` names an entry the inline forms would call more often
//! than that in a pipeline the program re-runs per re-open (srf/function_scan.rs refuses it by D1657's name); the rule's
//! one VOLATILE test is effects/visible/srf.rs `volatile` (effects' reader, with D1907's domain checks); `eager_build` and `hazard` name where the store the D1085 retirement serves refuses
//! D2107 (in-row, srf/function_store.rs). Registers nothing. Owner M5-SRF.
use gravity_decompose::lifetime;
use gravity_decompose::params;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::srf::{SrfDesc, SrfFn};
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, Source};
use gravity_program::ArenaRef;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::graph::Gx;

/// D1657's once rule (the one test; D1657's store route and D1085 reuse it): pipeline `k` is re-opened by an owner (an
/// embedded inner by its nest's or nested SubPlan's op; a rerun set's member by the set's owner, whose re-open runs it) and
/// a VOLATILE member of `d` is one the lifetime keep table rewinds at that owner's rescan (`lifetime::function_rescan`:
/// none of the owner's parameters meets its expression); with two owners, any VOLATILE member. `arena`: the lowering's,
/// for effects' walk.
pub(crate) fn reopen_once<'p>(gx: &Gx<'_, 'p>, arena: ArenaRef, k: usize, d: &SrfDesc<'p>) -> bool {
    let ps = gx.pipelines();
    let find = |node: i32, plan: i32| (0..ps.end()).filter_map(|q| ps.get(q)).flat_map(|p| p.ops.iter()).find(|op| owns(op, node, plan));
    let mut owners: PgVec<'_, PgVec<'_, i32>> = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(k as u16))).map(|r| find(r.node_id, r.plan_id).map_or_else(|| PgVec::new_in(gx.mcx), |op| params_of(op, gx.mcx))).collect_in(gx.mcx);
    if let Some(((q, j), _)) = k.checked_sub(ps.len()).and_then(|i| ps.inners().into_iter().nth(i)) {
        owners.push_in(params_of(&ps[q].ops[j], gx.mcx));
    }
    let vol = |f: &SrfFn<'p>| crate::effects::visible::srf::volatile(gx, arena, f.node);
    match owners.as_slice() {
        [] => false,
        [own] => d.functions.iter().any(|f| vol(f) && matches!(lifetime::function_rescan(f.node, own), lifetime::Keep::Keep { .. })),
        _ => d.functions.iter().any(vol),
    }
}

/// D2107 (in-row, W3c-D1085-RETIRE; the side class's fix is join/build_class.rs's, deferred to the bug phase).
pub(crate) const STORE_OUTER_FIRST_BUILD: &str = "a FROM function store the D1085 retirement serves, in a statement whose hash build on PG's outer-first path runs a nest, or in an InitPlan or CTE beside such a probe (the side class reads a nest as raising, so op 14 runs the build before the outer's first row; PG builds only then: D2107)";

/// D2107: whether the store of embedded inner `k` (D1657's once rule names it) may run where PG's Hash on the outer-first
/// path runs later or never (nodeHashjoin.c HJ_BUILD_HASHTABLE: the outer's first row, then the build). A probe on that
/// path whose build side (the build and its feeders) holds a nest or a nested SubPlan: join/build_class.rs `own_side`
/// reads either as raising without seeing into it, so op 14 runs that build before the outer's first row, an empty outer
/// too. Or the store computes (or feeds) an InitPlan or a CTE, which such a build may read first, and any such probe exists.
pub(crate) fn eager_build(gx: &Gx<'_, '_>, k: usize) -> bool {
    let (ps, inners) = (gx.pipelines(), gx.pipelines().inners());
    let mut top = k;
    while top >= ps.len() {
        top = inners[top - ps.len()].0 .0;
    }
    // pipeline `b` and every graph pipeline feeding it through a consumed slot or a breaker source, transitively
    let feeds = |b: usize| {
        let mut s = mcx::pg_vec![in gx.mcx; b];
        let mut i = 0;
        while i < s.len() {
            let x = s[i];
            let src = if let Source::Breaker { slot } = ps[x].source { Some(slot) } else { None };
            for l in ps[x].desc.consumes.iter().copied().chain(src).filter_map(|sl| gx.life_of(x, sl)).map(|l| l.0 as usize) {
                if l < ps.len() && !s.contains(&l) {
                    s.push_in(l);
                }
            }
            i += 1;
        }
        s
    };
    let nest = |x: usize| ps[x].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Nest { .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { .. }, .. })));
    let init = gx.graph.init_plans.iter().any(|i| feeds(i.pipeline as usize).contains(&top));
    let eager = |q: usize, op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Probe { slot, outer_first: true, .. }) if gx.life_of(q, *slot).is_some_and(|l| init || feeds(l.0 as usize).into_iter().any(&nest)));
    ps.iter().enumerate().any(|(q, p)| p.ops.iter().any(|op| eager(q, op)))
}

/// Whether `op` is the owner of rerun set (`node`, `plan`): the nest at plan node `node` (`plan` 0), or SubPlan `plan`.
fn owns(op: &Op<'_>, node: i32, plan: i32) -> bool {
    match op {
        Op::Join(JoinDesc::Nest { facts, .. }) => plan == 0 && facts.node == node,
        Op::SubPlan(s) => plan != 0 && s.plan_id == plan,
        _ => false,
    }
}

/// The parameters owner `op` sets before each re-open: a nest's NestLoopParams, a SubPlan's parParams.
fn params_of<'g>(op: &Op<'_>, mcx: Mcx<'g>) -> PgVec<'g, i32> {
    match op {
        Op::Join(JoinDesc::Nest { params: nest, .. }) => params::nest_sets(nest).collect_in(mcx),
        Op::SubPlan(s) => s.params.iter().collect_in(mcx),
        _ => PgVec::new_in(mcx),
    }
}

#[cfg(test)]
mod tests {
    use datum::Datum;
    use gravity_decompose::{NoFacts, PipelineGraph};
    use gravity_expr::catalog::{DomainConstraint, DomainFacts, FuncFacts, IoFacts, TextFacts, TypeFacts};
    use gravity_expr::{Catalog, PgCatalog};
    use gravity_fn::{Shim, Tier};
    use gravity_program::Arena;
    use mcx::MemoryContext;
    use types_core::Oid;
    use types_error::{PgError, PgResult};
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{CoerceToDomain, CoerceToDomainValue, CoercionForm, FuncExpr};
    use types_nodes::Node;

    use crate::graph::Gx;

    /// `PgCatalog` with the test's domains (a unit test has no syscache): each domain's constraints, `None` a NOT NULL;
    /// a domain it does not list is unreadable.
    struct Doms(Vec<(Oid, Vec<Option<Node<'static>>>)>);

    impl Catalog<'static> for Doms {
        fn domain(&self, t: Oid) -> PgResult<DomainFacts<'static>> {
            let (_, cs) = self.0.iter().find(|d| d.0 == t).ok_or_else(|| Box::new(PgError::error("no such domain")))?; // alloc: Cold
            let constraints = cs.iter().map(|c| c.map_or(DomainConstraint::NotNull, |expr| DomainConstraint::Check { name: "c", expr })).collect();
            Ok(DomainFacts { typlen: 4, constraints })
        }
        fn func(&self, f: Oid) -> PgResult<FuncFacts> {
            Catalog::<'static>::func(&PgCatalog, f)
        }
        fn opcode(&self, o: Oid) -> PgResult<Oid> {
            Catalog::<'static>::opcode(&PgCatalog, o)
        }
        fn op_input_types(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'static>::op_input_types(&PgCatalog, o)
        }
        fn btree_cmp_proc(&self, f: Oid, l: Oid, r: Oid) -> PgResult<Oid> {
            Catalog::<'static>::btree_cmp_proc(&PgCatalog, f, l, r)
        }
        fn opclass(&self, o: Oid) -> PgResult<(Oid, Oid)> {
            Catalog::<'static>::opclass(&PgCatalog, o)
        }
        fn type_facts(&self, t: Oid) -> PgResult<TypeFacts> {
            Catalog::<'static>::type_facts(&PgCatalog, t)
        }
        fn type_cmp_proc(&self, t: Oid) -> PgResult<Oid> {
            Catalog::<'static>::type_cmp_proc(&PgCatalog, t)
        }
        fn base_type(&self, t: Oid, m: &mut i32) -> PgResult<Oid> {
            Catalog::<'static>::base_type(&PgCatalog, t, m)
        }
        fn io(&self, t: Oid) -> PgResult<IoFacts> {
            Catalog::<'static>::io(&PgCatalog, t)
        }
        fn text(&self, c: Oid) -> PgResult<TextFacts> {
            Catalog::<'static>::text(&PgCatalog, c)
        }
        fn tier(&self, f: Oid) -> Tier {
            Catalog::<'static>::tier(&PgCatalog, f)
        }
        fn shim(&self, f: Oid) -> Option<&'static Shim> {
            Catalog::<'static>::shim(&PgCatalog, f)
        }
    }

    /// D1907: a CoerceToDomain's CHECK acts exactly when it holds a VOLATILE call (through a domain cast inside a check
    /// too), or the catalog cannot read the domain, or the domain is met again inside its own checks; an IMMUTABLE or
    /// STABLE check and a NOT NULL alone do not (builtins from decompose's facts: abs(int4) 1397 IMMUTABLE, now() 1299
    /// STABLE, nextval 1574 and random() 1598 VOLATILE). Q4 fails closed above it: every domain coercion counts as VOLATILE.
    #[test]
    fn a_domain_counts_as_volatile_when_its_check_holds_a_volatile_call_or_cannot_be_read() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("d1907"))); // alloc: Cold
        let m = cx.mcx();
        let value = || Node::mk(m, CoerceToDomainValue { typeId: 23, typeMod: -1, collation: 0, location: -1 }).expect("VALUE");
        let call = |funcid: Oid, x: Node<'static>| Node::mk(m, FuncExpr { funcid, funcresulttype: 23, args: NodeList::make1(m, x).expect("args"), ..Default::default() }).expect("a call");
        let cast = |t: Oid, arg: Node<'static>| Node::mk(m, CoerceToDomain { arg, resulttype: t, resulttypmod: -1, resultcollid: 0, coercionformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).expect("a domain cast");
        let doms = Doms(vec![ // alloc: Cold
            (9001, vec![Some(call(1397, value()))]), // alloc: Cold
            (9002, vec![None, Some(call(1299, value()))]), // alloc: Cold
            (9003, vec![Some(call(1397, value())), Some(call(1574, value()))]), // alloc: Cold
            (9004, vec![None]), // alloc: Cold
            (9005, vec![Some(call(1397, cast(9003, value())))]), // alloc: Cold
            (9006, vec![Some(call(1397, cast(9006, value())))]), // alloc: Cold
            (9007, vec![Some(call(1397, cast(9001, value())))]), // alloc: Cold
            (9008, vec![Some(call(1598, value()))]), // alloc: Cold
        ]);
        let catalog: &'static dyn Catalog<'static> = Box::leak(Box::new(doms)); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 12))); // alloc: Cold
        let g: &'static PipelineGraph = Box::leak(Box::new(PipelineGraph::new(arena.handle()))); // alloc: Cold
        let gx = Gx { graph: g, facts: &NoFacts, catalog, over: None, mcx: rig.mcx() };
        let three = Node::mk_const(m, 23, -1, 0, 4, Datum::from_i32(3), false, true).expect("a const");
        let cx = gravity_decompose::effects::Cx::without_stmt(gx.facts, arena.handle());
        // D1907's CHECK reading alone; 9007's CHECK holds a cast to 9001, which effects reads as acting (Q4 below).
        for (t, want) in [(9001, false), (9002, false), (9003, true), (9004, false), (9005, true), (9006, true), (9007, true), (9008, true), (9999, true)] {
            assert_eq!(crate::effects::check_acts(&gx, &cx, cast(t, three)), want, "the CHECK of domain {t}");
            assert_eq!(crate::effects::check_acts(&gx, &cx, call(1397, cast(t, three))), want, "the CHECK of domain {t} under abs");
        }
        // Q4 fail closed: effects reads every domain coercion as acting until the facts carry a domain's CHECKs, so the
        // VOLATILE test holds for each domain, an IMMUTABLE or STABLE check and a NOT NULL alone too.
        for t in [9001, 9002, 9003, 9004, 9005, 9006, 9007, 9008, 9999] {
            assert!(crate::effects::visible::srf::volatile(&gx, arena.handle(), cast(t, three)), "domain {t}");
            assert!(crate::effects::visible::srf::volatile(&gx, arena.handle(), call(1397, cast(t, three))), "abs over domain {t}");
        }
        assert!(!crate::effects::visible::srf::volatile(&gx, arena.handle(), call(1397, three)), "no domain, an IMMUTABLE call");
    }
}
