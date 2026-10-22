//! D1016 (tour-scans scan-0128): the admission and BIND of a TID scan whose value is computed from executor parameters
//! (hosts ops/scan/tid_param.rs): a SubPlan's `c.ctid >= format('(%s,0)', p.pg)::tid`, a nest inner's `ctid = (o.t)::tid`.
//! M2's TID arm leaves such a site (sites_heap.rs `tid`, by `ours`); this arm builds M2's TID recipe and files it with each
//! computed value's clause cell and the parameters the values read (lowerv4 ops/scan/tid.rs: the clause cells, then the
//! input cells, params_key.rs `key_ids` order). BIND compiles each value once per execution with PostgreSQL's
//! ExecInitExpr over this execution's extern parameters and the recipe's `ExecVals` (as ops/scan/param_key.rs `bind`);
//! the host evaluates them at every (re-)open, as TidListEval / TidRangeEval do at the first fetch after each rescan. A
//! scan outside the main graph and a value the executor cannot initialize are refused by name.
use std::rc::Rc;
use std::sync::Arc;

use gravity_decompose::ops::shared::scan::TidForm;
use gravity_decompose::{Am, Source};
use gravity_hosts::ops::scan::bitmap_param::ExecVals;
use gravity_hosts::ops::scan::tid_param::ParamTidRecipe;
use gravity_hosts::ops::scan::tid_sample::TidRecipe;
use gravity_hosts::ops::shared::key_fn::KeyFn;
use gravity_hosts::HostRegistry;
use gravity_lowerv4::rerun::params_key::key_ids_by;
use types_core::Oid;
use types_nodes::{Node, NodeList};
use types_portal::params::ParamBind;
use types_storage::lock::NoLock;

use crate::admit::{refuse, AdmitCx, AdmitResult, BindCx, BindResult, RecipeTpl, TplVec};
use crate::ops::scan::param_key::SameIds;
use crate::shapes;

/// The template ADMIT files: M2's TID recipe, each computed value's clause cell, the parameters the values read (in input
/// cell order) and the TID scan's plan node.
/// memsig E-prog (C): its lists are built in place in the recipe set's root (`AdmitCx::lists`).
pub(super) struct Tpl {
    leaf: TidRecipe,
    words: TplVec<u16>,
    ids: TplVec<i32>,
    node_id: i32,
}

/// The value side of TID clause `q` (lowerv4 ops/scan/tid.rs `value_of`): `ctid op v`, `v op ctid`, `ctid = ANY (a)`.
fn value_of<'p>(q: Node<'p>, varno: i32) -> Option<Node<'p>> {
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == -1);
    if let Some(op) = q.as_op_expr().filter(|o| o.args.len() == 2) {
        return match (is_ctid(op.args.nth(0)), is_ctid(op.args.nth(1))) {
            (true, _) => Some(op.args.nth(1)),
            (_, true) => Some(op.args.nth(0)),
            _ => None,
        };
    }
    q.as_scalar_array_op_expr().filter(|s| s.args.len() == 2 && s.useOr && is_ctid(s.args.nth(0))).map(|s| s.args.nth(1))
}

/// Whether clause `w`'s value `v` is the host's: computed from executor parameters, or (D1630) bound at bind on an inner
/// walk, whose site slot for it is negative (lowerv4 lifetime/bound.rs `host_at`).
fn hosted(v: Node<'_>, w: u16, host: &dyn Fn(u16) -> bool) -> bool {
    gravity_lowerv4::lifetime::bound::host_at(v, host(w))
}

/// The host's values of a TID site's clauses in clause (key cell) order: each one's cell and expression to `out`, and
/// the parameters they read to `id` as the key walk meets them (lowerv4 params_key.rs `key_ids_by`: kept once each, in
/// that order, they are the input cells'); `false` when a value reads a node the key walk does not know. memsig E-prog:
/// no list of its own (`admit` builds the template's in place, `bind` checks as it walks).
fn computed_values<'p>(quals: &'p NodeList<'p>, varno: i32, host: &dyn Fn(u16) -> bool, out: &mut dyn FnMut(u16, Node<'p>), id: &mut dyn FnMut(i32)) -> bool {
    for (w, q) in quals.iter().enumerate() {
        let Some(v) = value_of(q, varno).filter(|&v| hosted(v, w as u16, host)) else { continue };
        if !key_ids_by(v, id) {
            return false;
        }
        out(w as u16, v);
    }
    true
}

/// Whether a TID site is this arm's (sites_heap.rs `tid` declines it): some clause value the host's (`hosted`; `host`: the
/// site's slot for the clause is negative).
pub(crate) fn ours(form: TidForm, quals: &NodeList<'_>, varno: i32, host: &dyn Fn(u16) -> bool) -> bool {
    form != TidForm::Current && quals.iter().enumerate().any(|(w, q)| value_of(q, varno).is_some_and(|v| hosted(v, w as u16, host)))
}

/// The admission's `host` predicate: the site's param slot for clause cell `w` is negative.
pub(crate) fn host_slot<'c>(cx: &'c AdmitCx<'_>) -> impl Fn(u16) -> bool + 'c {
    |w: u16| cx.site.param_slots.get(w as usize).is_some_and(|&s| s < 0)
}

/// A TID site with a computed value: M2's recipe, the values' cells and parameters, or `None` (M2's arm).
pub(crate) fn admit(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Tid { form, quals, scanrelid, am: Am::Heap, .. } = cx.source else { return Ok(Ok(None)) };
    let host = host_slot(cx);
    if !ours(*form, quals, *scanrelid as i32, &host) {
        return Ok(Ok(None));
    }
    // memsig E-prog (C): the template's lists, built in place in the recipe set's root (a refusal drops the set whole)
    let (mut words, mut ids) = (cx.lists.list_with(quals.len()), cx.lists.list());
    if !computed_values(quals, *scanrelid as i32, &host, &mut |w, _| cx.lists.push(&mut words, w), &mut |id| if !ids.contains(&id) { cx.lists.push(&mut ids, id) }) {
        return Ok(refuse("hosts", "a tid value computed from executor parameters over a node the key walk does not know (D1016)"));
    }
    let mut clauses = Vec::with_capacity(quals.len()); // alloc: Program
    for q in quals.iter() {
        match crate::ops::scan::sites_heap::tid_clause(q, *scanrelid as i32, *form) {
            Some(c) => clauses.push(c), // alloc: Program
            None => return Ok(refuse("hosts", "a tid clause outside the tid comparison set")),
        }
    }
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let recipe = TidRecipe::from_plan(&heap, cx.attnos, clauses, *form == TidForm::Range);
    heap.close(NoLock)?;
    Ok(recipe.map(|leaf| Some(RecipeTpl::Rung(Arc::new(Tpl { leaf, words, ids, node_id: cx.node_id })))).or_else(|e| refuse("hosts", e.message()))) // alloc: Program
}

/// A computed-value template into this execution's registry: each value compiled once over this execution's parameters.
pub(crate) fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> BindResult {
    let RecipeTpl::Rung(any) = t else { return Ok(Ok(false)) };
    let Some(t) = any.downcast_ref::<Tpl>() else { return Ok(Ok(false)) };
    // leader only (the host's `ExecVals` and compiled values are thread-confined): a TID pipeline always runs serial on the
    // leader (lowerv4 ops/scan/tid.rs `serial`: a range's eligible descriptor turns Ineligible), so only a scan outside
    // the main graph refuses
    let mut all = cx.charged_vec();
    all.with_vec(|v| cx.graph.iter().flat_map(|g| g.pipelines.iter()).for_each(|p| crate::admit::sources(p, v))); // alloc: Exec charged cx.owner
    let Some(&(src, _)) = all.iter().find(|&&(_, id)| id == t.node_id) else {
        return Ok(refuse("hosts", "a tid scan with a value computed from executor parameters outside the main graph"));
    };
    // this execution's computed values (their nodes live for the execution), in the admission's layout
    // memsig E-prog (A): the values in a list on this bind's owner, their parameters checked as walked (`SameIds`)
    let (mut values, mut ids) = (cx.charged_vec(), SameIds::new(&t.ids));
    let found = match src {
        Source::Tid { quals, scanrelid, .. } => computed_values(quals, *scanrelid as i32, &|w| t.words.contains(&w), &mut |w, v| values.with_vec(|l| l.push((w, v))), &mut |id| ids.meet(id)), // alloc: Exec charged cx.owner
        _ => false,
    };
    if !(found && ids.all() && values.iter().map(|&(w, _)| w).eq(t.words.iter().copied())) {
        return Ok(refuse("hosts", "a tid scan whose computed values are not the admission's"));
    }
    let vals = ExecVals::new(t.ids.iter().copied().max().unwrap_or(0).max(0) as usize + 1);
    let (ptr, n) = vals.bind();
    let owner = std::thread::current().id();
    let mut keys: Vec<(u16, KeyFn)> = Vec::with_capacity(values.len()); // alloc: Program
    for &(w, v) in values.iter() {
        let bind = ParamBind { exec_vals: Some(ptr), n_exec: n, ..shapes::param_bind(exprs) };
        let Some(st) = execexpr::exec_init_expr(cx.mcx, Some(v), bind)? else {
            return Ok(refuse("hosts", "a tid value computed from executor parameters the executor did not initialize"));
        };
        let cell = core::cell::RefCell::new(st);
        let f: KeyFn = Rc::new(move |mcx| {
            // D1356's rule for a compiled expression: thread-confined (the TID scan runs on the leader)
            debug_assert_eq!(std::thread::current().id(), owner, "gravity: a computed tid value evaluated off its creating thread (D1016)");
            let mut st = cell.borrow_mut();
            st.arm_result_mcx(mcx);
            let nd = execexpr::exec_eval_expr(&mut *st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
            Ok((nd.value.as_u64(), nd.isnull))
        }); // alloc: Program
        keys.push((w, f)); // alloc: Program
    }
    // keys.rs rebinds the extern parameters of an Admitted that compiled an expression over them
    exprs.filters += 1;
    let base = t.leaf.nkeys() as u16;
    let inputs: Vec<(u16, u16)> = t.ids.iter().enumerate().map(|(i, &id)| (base + i as u16, id as u16)).collect(); // alloc: Program
    reg.push_rung(Box::new(ParamTidRecipe { leaf: t.leaf.clone(), keys: keys.into(), vals, inputs: inputs.into() })); // alloc: Program
    Ok(Ok(true))
}
