//! D1016 row 3 (V4R-547): the admission and BIND of a TABLESAMPLE whose argument is computed from executor parameters
//! (hosts v4/m8_param_sample.rs): `bernoulli (pct)` over a LATERAL VALUES column cast to float4, `system (t1.b)` under a
//! Memoize. M2's sample arm leaves such a site (m2_admit.rs `sample`, by `ours`); this arm builds M2's recipe (the heap
//! range recipe and the method) and files it with the one executor parameter the argument reads (lowerv4 source/sample.rs:
//! the site's key cells are the host's argument cell, then that parameter's input cell). BIND compiles the argument once
//! per execution with PostgreSQL's ExecInitExpr over this execution's extern parameters and the recipe's `ExecVals` (as
//! m8_param_key.rs `bind`); the host evaluates it at every (re-)open, as nodeSamplescan.c's tablesample_init does at the
//! first fetch after each rescan. Another layout, a walk in a parallel-eligible pipeline or outside the main graph, and an
//! argument the executor cannot initialize are refused by name. Created by M8-NEST.
use std::rc::Rc;
use std::sync::Arc;

use gravity_decompose::ops::scan::closure::SampleDesc;
use gravity_decompose::{Am, Source};
use gravity_hosts::ops::scan::tid_sample::SampleRecipe;
use gravity_hosts::ops::scan::bitmap_param::ExecVals;
use gravity_hosts::ops::scan::sample_param::ParamSampleRecipe;
use gravity_hosts::ops::shared::key_fn::KeyFn;
use gravity_hosts::{HostRegistry, RangeRecipe};
use gravity_lowerv4::rerun::params_key::{computed, key_ids_by};
use gravity_program::Parallel;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::Node;
use types_portal::params::ParamBind;
use types_storage::lock::NoLock;

use crate::admit::{refuse, AdmitCx, AdmitResult, BindCx, BindResult, RecipeTpl, TplVec};
use crate::ops::scan::param_key::SameIds;
use crate::shapes;

/// The template ADMIT files: M2's sample recipe (REPEATABLE or not), the parameters the computed values read and the
/// sample scan's plan node.
pub(crate) struct Tpl {
    leaf: SampleRecipe,
    /// memsig E-prog (C): built in place in the recipe set's root (`AdmitCx::lists`).
    ids: TplVec<i32>,
    node_id: i32,
}

// Send / Sync: ops/scan/raw.rs (a template lives in its session thread's backend cache).

/// Whether a sample site is this arm's (m2_admit.rs declines it): an argument or REPEATABLE computed from executor
/// parameters (the lowerer's own predicate, params_key.rs `computed`) or evaluated at each open (D1643: one that is
/// VOLATILE or may raise, decompose `SampleDesc::per_open`).
pub(crate) fn ours(sample: &SampleDesc<'_>) -> bool {
    sample.args.iter().chain(sample.repeatable).any(|v| hosted(sample, v))
}

/// Whether value `v` of `sample` is the host's, evaluated at each open (lowerv4 source/sample.rs): computed from executor
/// parameters, or VOLATILE or raising (D1643).
fn hosted(sample: &SampleDesc<'_>, v: Node<'_>) -> bool {
    computed(v) || sample.opens(v)
}

/// The layout the lowerer serves (source/sample.rs): one argument and an optional REPEATABLE, each a key cell (a word or an
/// executor parameter's) or computed (the host's, evaluated at each open); the parameters the computed ones read to `id`
/// as the key walk meets them, argument then REPEATABLE (lowerv4 params_key.rs `key_ids_by`: kept once each, in that
/// order, they are the site's input cells after the key cells), or `false`. memsig E-prog: no list of its own.
fn layout(sample: &SampleDesc<'_>, id: &mut dyn FnMut(i32)) -> bool {
    let mut args = sample.args.iter();
    let (Some(_), None) = (args.next(), args.next()) else { return false };
    sample.args.iter().chain(sample.repeatable).filter(|&v| computed(v)).all(|v| key_ids_by(v, id))
}

/// `sample`'s layout is template `t`'s: the same parameters in the same order (`SameIds`, no list).
fn same_layout(sample: &SampleDesc<'_>, t: &Tpl) -> bool {
    let mut ids = SameIds::new(&t.ids);
    layout(sample, &mut |id| ids.meet(id)) && ids.all()
}

/// A heap sample with a computed argument: M2's recipe and the argument's parameter, or `None` (M2's arm).
pub(crate) fn admit(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Sample { sample, am: Am::Heap, .. } = cx.source else { return Ok(Ok(None)) };
    if !ours(sample) {
        return Ok(Ok(None));
    }
    if cx.site.rows_via_am {
        return Ok(refuse("hosts", "a TABLESAMPLE on the rows road with an argument computed from executor parameters (D4735)"));
    }
    // memsig E-prog (C): the parameter list built in place in the recipe set's root (a refusal drops the set whole)
    let mut ids = cx.lists.list();
    if !layout(sample, &mut |id| if !ids.contains(&id) { cx.lists.push(&mut ids, id) }) {
        return Ok(refuse("hosts", "a TABLESAMPLE method taking other than one argument, or a computed value over a node the key walk does not know (D1016 row 3)"));
    }
    // the by-reference lanes as a bit set (m2_admit.rs `lane_words`)
    let mut byref = [0u64; 64];
    cx.site.lanes().iter().enumerate().take(cx.site.natts().min(64 * 64)).filter(|(_, l)| l.kind == gravity_abi::ctx::lane::KIND_BYREF).for_each(|(k, _)| byref[k / 64] |= 1 << (k % 64));
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let range = RangeRecipe::from_plan_byref(&heap, cx.attnos, &byref);
    heap.close(NoLock)?;
    let recipe = range.and_then(|range| SampleRecipe::from_plan(range, cx.mcx, sample.handler as Oid, sample.repeatable.is_some(), sample.args as *const _ as usize));
    Ok(recipe.map(|leaf| Some(RecipeTpl::Rung(Arc::new(Tpl { leaf, ids, node_id: cx.node_id })))).or_else(|e| refuse("hosts", e.message()))) // alloc: Program
}

/// A computed-argument template into this execution's registry: the argument compiled once over this execution's parameters.
pub(crate) fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> BindResult {
    let RecipeTpl::Rung(any) = t else { return Ok(Ok(false)) };
    let Some(t) = any.downcast_ref::<Tpl>() else { return Ok(Ok(false)) };
    // leader only (the host's `ExecVals` and compiled argument are thread-confined), as m8_param_key.rs `bind`
    let walk = cx.graph.and_then(|g| g.pipelines.iter().find_map(|p| crate::ops::shared::walk::walk_of(p, t.node_id)));
    match walk.map(|p| matches!(p.desc.parallel, Parallel::Eligible)) {
        Some(false) => {}
        Some(true) => return Ok(refuse("hosts", "a TABLESAMPLE with an argument computed from executor parameters in a parallel-eligible pipeline")),
        None => return Ok(refuse("hosts", "a TABLESAMPLE with an argument computed from executor parameters outside the main graph")),
    }
    // this execution's argument and REPEATABLE (their nodes live for the execution), in the admission's layout
    let sample = walk.and_then(|p| match &p.source {
        Source::Sample { sample, .. } if same_layout(sample, t) => Some(sample),
        _ => None,
    });
    let Some(sample) = sample else {
        return Ok(refuse("hosts", "a TABLESAMPLE whose computed argument is not the admission's"));
    };
    let vals = ExecVals::new(t.ids.iter().map(|&i| i.max(0) as usize + 1).max().unwrap_or(1));
    let (ptr, n) = vals.bind();
    let owner = std::thread::current().id();
    // a computed value compiled once per execution with PG's ExecInitExpr; the host evaluates it at each open
    let mut fns: [Option<KeyFn>; 2] = [None, None];
    for (slot, v) in fns.iter_mut().zip([sample.args.iter().next(), sample.repeatable]) {
        let Some(v) = v.filter(|&v| hosted(sample, v)) else { continue };
        let bind = ParamBind { exec_vals: Some(ptr), n_exec: n, ..shapes::param_bind(exprs) };
        let Some(st) = execexpr::exec_init_expr(cx.mcx, Some(v), bind)? else {
            return Ok(refuse("hosts", "a TABLESAMPLE argument computed from executor parameters the executor did not initialize"));
        };
        let cell = core::cell::RefCell::new(st);
        let f: KeyFn = Rc::new(move |mcx| {
            // D1356's rule for a compiled expression: thread-confined (the sample runs on the leader)
            debug_assert_eq!(std::thread::current().id(), owner, "gravity: a computed TABLESAMPLE value evaluated off its creating thread (D1016 row 3)");
            let mut st = cell.borrow_mut();
            st.arm_result_mcx(mcx);
            let nd = execexpr::exec_eval_expr(&mut *st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
            Ok((nd.value.as_u64(), nd.isnull))
        }); // alloc: Program
        *slot = Some(f);
    }
    let [arg, rep] = fns;
    // keys.rs rebinds the extern parameters of an Admitted that compiled an expression over them
    exprs.filters += 1;
    let ids = t.ids.iter().map(|&i| i as u16).collect(); // alloc: Program
    let leaf = t.leaf.bound(reg); // D1920 / D1644: the plan node's one seed in this execution
    reg.push_rung(Box::new(ParamSampleRecipe { leaf, arg, rep, vals, ids })); // alloc: Program
    Ok(Ok(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_decompose::ops::scan::closure::SampleMethod;
    use types_nodes::primnodes::{CoercionForm, OpExpr, Param, ParamKind};
    use types_nodes::{Node, NodeList};

    /// D1016 row 3: a computed argument or REPEATABLE is this arm's (m2_admit.rs declines it); a constant or a bare
    /// parameter stays M2's; the served layout lists each parameter the computed values read once, argument then REPEATABLE.
    #[test]
    fn a_computed_argument_is_this_arms() {
        let ctx = mcx::MemoryContext::new("d1016-row3");
        let mcx = ctx.mcx();
        let param = |kind, paramid| Node::mk(mcx, Param { paramkind: kind, paramid, paramtype: types_core::INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let (p0, p1, x) = (param(ParamKind::PARAM_EXEC, 0), param(ParamKind::PARAM_EXEC, 1), param(ParamKind::PARAM_EXTERN, 1));
        let cast = |p| Node::mk_relabel_type(mcx, p, types_core::FLOAT4OID, -1, 0, CoercionForm::COERCE_IMPLICIT_CAST).unwrap();
        let c = Node::mk_const(mcx, types_core::FLOAT4OID, -1, 0, 4, datum::Datum::from_u64(0), false, true).unwrap();
        let two = |a, b| Node::mk(mcx, OpExpr { opno: 551, opfuncid: 0, opresulttype: types_core::INT4OID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, a, b).unwrap(), location: -1 }).unwrap();
        let args: Vec<NodeList<'_>> = [c, p0, x, cast(p0), two(p0, p1)].iter().map(|&a| NodeList::make1(mcx, a).unwrap()).collect();
        let s = |i: usize, repeatable| SampleDesc { method: SampleMethod::Bernoulli, handler: 0, args: &args[i], repeatable, per_open: 0 };
        // `layout`'s parameters kept once each, as `admit` keeps them
        let layout = |d: &SampleDesc<'_>| {
            let mut ids = Vec::new(); // alloc: Cold
            let ok = super::layout(d, &mut |id| if !ids.contains(&id) { ids.push(id) }); // alloc: Cold
            ok.then_some(ids)
        };
        assert!(!ours(&s(0, None)) && !ours(&s(1, None)) && !ours(&s(2, None)), "a constant, a bare executor or extern parameter stay M2's");
        assert!(!ours(&s(0, Some(p0))), "a bare REPEATABLE parameter stays M2's");
        assert!(ours(&s(3, None)) && ours(&s(0, Some(cast(p0)))), "a computed argument or REPEATABLE is this arm's");
        assert_eq!(layout(&s(3, None)).as_deref(), Some(&[0][..]), "one computed argument over one parameter");
        assert_eq!(layout(&s(3, Some(c))).as_deref(), Some(&[0][..]), "a constant REPEATABLE beside a computed argument");
        assert_eq!(layout(&s(4, None)).as_deref(), Some(&[0, 1][..]), "two executor parameters, each once");
        assert_eq!(layout(&s(0, Some(cast(p0)))).as_deref(), Some(&[0][..]), "a computed REPEATABLE");
        assert_eq!(layout(&s(3, Some(cast(p0)))).as_deref(), Some(&[0][..]), "the same parameter in both, once");
        // D1643: a value decompose evaluates at each open (VOLATILE or may raise, over no executor parameter) is this arm's,
        // the host's with no parameter; unmarked, the same value stays M2's word
        let k = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_u64(7), false, true).unwrap();
        let loud = NodeList::make1(mcx, two(k, k)).unwrap();
        let d = SampleDesc { method: SampleMethod::Bernoulli, handler: 0, args: &loud, repeatable: None, per_open: 1 };
        assert!(ours(&d) && layout(&d).as_deref() == Some(&[][..]), "a per-open argument is this arm's, reading no parameter");
        assert!(!ours(&SampleDesc { per_open: 0, ..d }), "an unmarked one stays M2's");
        assert!(ours(&SampleDesc { repeatable: Some(two(k, k)), per_open: 2, ..s(0, None) }), "a per-open REPEATABLE (bit 1)");
    }
}
