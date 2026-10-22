//! D1016 (M6-REPS row 1): the admission and BIND of a btree index walk (or index-only walk) with a key computed from
//! executor parameters (hosts ops/scan/param_key.rs): an array element, a regproc column, an arithmetic expression
//! over the outer row's columns (`i.k = o.a + o.b`), row 2's `i.k = ANY (ARRAY[o.a, o.b])`. M1's and M2's arms leave such a site (m1_admit.rs
//! `m1_index`, m2_admit.rs `bound_array`); this arm builds M1's btree recipe (`from_plan_words`: the recipe never reads a
//! key's argument, an array's word is SK_SEARCHARRAY's) and files it with each computed key's plan expression and
//! the parameters the keys read (lowerv4 params_key.rs `key_ids`, the order of the site's input cells). BIND compiles each key
//! once per execution with PostgreSQL's ExecInitExpr, over this execution's extern parameters and the recipe's
//! executor-parameter array (Row A's `ExecVals`), and the host evaluates it at every seek, as ExecReScan's
//! ExecIndexEvalRuntimeKeys does. A key the executor cannot initialize, and a walk in a parallel-eligible pipeline the
//! program runs wide, are refused by name. Row 4 (V4R-557): a hash / GiST / SP-GiST walk, over M2's recipe (`am_recipe`). Created by M6-REPS.
use std::rc::Rc;
use std::sync::Arc;

use gravity_decompose::graph::KeyForm;
use gravity_decompose::{IndexAm, Source};
use gravity_hosts::ops::scan::bitmap_param::ExecVals;
use gravity_hosts::ops::scan::param_key::ParamKeyRecipe;
use gravity_hosts::ops::shared::key_fn::KeyFn;
use gravity_hosts::{HostRegistry, SiteRecipe};
use gravity_program::Parallel;
use types_core::Oid;
use types_nodes::Node;
use types_portal::params::ParamBind;
use types_storage::lock::NoLock;

use crate::admit::{refuse, AdmitCx, AdmitResult, AdmitTpl, BindCx, BindResult, RecipeTpl, TplVec};
use crate::ops::shared::walk::walk_of;
use crate::shapes;

/// The template M8's ADMIT files (`RecipeTpl::Rung`): the btree recipe, each computed key's word, the parameters the
/// input cells carry (in cell order) and the walk's plan node. memsig E-prog (C): its lists are built in place in the
/// recipe set's root (`AdmitCx::lists`).
pub(super) struct Tpl {
    leaf: SiteRecipe,
    /// Each computed key's word: its expression is read again at BIND from this execution's graph (the admission's graph
    /// arena is reset after admission, so a node it built, an InitPlan's parameter rewrite, never outlives it), or on a
    /// front-key hit from this execution's plan node (`plan`).
    words: TplVec<u16>,
    ids: TplVec<i32>,
    node_id: i32,
    /// D2754: what a front-key hit, bound without the graph, reads instead (`plan_keys`); `None` when the walk is outside
    /// the main graph or a key is not one of its plan node's own expressions (decompose built it: an InitPlan parameter
    /// rewrite). Then the template binds from the graph alone and the front never files its statement (admit/mod.rs
    /// `graph_bound`).
    plan: Option<PlanKeys>,
    /// D3295: the program runs the walk's pipeline on the leader (`AdmitCx::leader`; lowerv4 params_key.rs
    /// `host_keys_serial`), though the graph's descriptor is parallel-eligible.
    leader: bool,
}

/// D2754: the walk's pipeline is parallel-eligible (`parallel`), and each computed key, in `words` order, is the node at
/// pre-order position `.1` of list `.0` of its plan node's index clauses (`clause_lists`), of tag `.2`. A statement the
/// front key files has the same plan shape at every execution, so the same positions hold that execution's own key
/// expressions; a tag that differs refuses (fail closed).
struct PlanKeys {
    parallel: bool,
    at: TplVec<(usize, usize, types_nodes::NodeTag)>,
}

/// Whether `t` is a computed-key template a front-key hit binds from the statement's plan (`Tpl::plan`): admit/mod.rs
/// `graph_bound` takes it out of D411's graph-bound recipes (scan/front_kind.rs answers any rung it does not know as one).
pub(crate) fn plan_keyed(t: &RecipeTpl) -> bool {
    matches!(t, RecipeTpl::Rung(any) if any.downcast_ref::<Tpl>().is_some_and(|t| t.plan.is_some()))
}

/// The index clause lists of plan node `n` a computed key may sit in: the index quals, the ORDER BY operands, and the
/// original quals (an IndexScan's `indexqualorig`, an IndexOnlyScan's `recheckqual`).
fn clause_lists(n: Node<'static>) -> Option<[&'static types_nodes::NodeList<'static>; 3]> {
    n.as_index_scan().map(|s| [&s.indexqual, &s.indexorderby, &s.indexqualorig]).or_else(|| n.as_index_only_scan().map(|s| [&s.indexqual, &s.indexorderby, &s.recheckqual]))
}

/// Each node of `l`, clause by clause, in `expr::walk`'s pre-order.
fn preorder(l: &types_nodes::NodeList<'static>, f: &mut dyn FnMut(Node<'static>)) {
    for c in l.iter() {
        gravity_decompose::expr::walk(c, &mut |x| {
            f(x);
            true
        });
    }
}

/// D2754: `PlanKeys` for the walk at plan node `cx.node_id` with computed keys `keys` (the graph's, `n` of them: `admit`'s
/// count), or `None` (the template stays graph-bound): its pipeline found in the admitting graph as `bind` finds it, and
/// each key found by identity among its plan node's clause lists. memsig E-prog (C): `at` is built in place in the
/// recipe set's root.
fn plan_keys(cx: &AdmitCx<'_>, keys: &[gravity_decompose::IndexKey<'_>], host: &dyn Fn(u16) -> bool, n: usize) -> Option<PlanKeys> {
    let parallel = cx.graph.pipelines.iter().find_map(|p| walk_of(p, cx.node_id)).map(|p| matches!(p.desc.parallel, Parallel::Eligible))?;
    let lists = clause_lists(plan_node(cx.pstmt, cx.node_id)?)?;
    let find = |v: Node<'_>| {
        lists.iter().enumerate().find_map(|(li, l)| {
            let (mut i, mut at) = (0usize, None);
            preorder(l, &mut |x| {
                if at.is_none() && x.ptr_eq(v) {
                    at = Some(i);
                }
                i += 1;
            });
            at.map(|i| (li, i, v.node_tag()))
        })
    };
    let (mut at, mut all) = (cx.lists.list_with(n), true);
    computed_keys(keys, host, &mut |_, v| match find(v) {
        Some(x) if all => cx.lists.push(&mut at, x),
        _ => all = false,
    }, &mut |_| {})?;
    (all && at.len() == n).then_some(PlanKeys { parallel, at })
}

/// D2754: the computed keys a front-key hit binds, read from this execution's plan node at `k`'s positions, each to
/// `out` with the parameters it reads to `id` (as `computed_keys` answers from the graph); `false` when a position is
/// missing or holds another tag.
fn plan_found(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, t: &Tpl, k: &PlanKeys, out: &mut dyn FnMut(u16, Node<'static>), id: &mut dyn FnMut(i32)) -> bool {
    let Some(lists) = plan_node(pstmt, t.node_id).and_then(clause_lists) else { return false };
    let mut n = 0usize;
    for (&w, &(li, at, tag)) in t.words.iter().zip(k.at.iter()) {
        let (mut i, mut got) = (0usize, None);
        preorder(lists[li], &mut |x| {
            if i == at {
                got = Some(x);
            }
            i += 1;
        });
        let Some(v) = got.filter(|v| v.node_tag() == tag) else { return false };
        if !gravity_lowerv4::rerun::params_key::key_ids_by(v, id) {
            return false;
        }
        out(w, v);
        n += 1;
    }
    n == t.words.len()
}

/// An index walk with a key computed from executor parameters: its recipe and keys, or `None` (another arm's).
pub(crate) fn admit(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Index { scanrelid, indexid, backward, only, keys, iam, .. } = cx.source else { return Ok(Ok(None)) };
    // the key words in the host's order (hosts `SiteRecipe::words`, lowerv4 source/index.rs): a comparison's value, a row
    // comparison's members' values, a null test none
    // D1630: a key bound at bind is the host's on an inner walk, whose site slot for it is negative (lowerv4 params_key.rs
    // `per_open`); elsewhere it has its word's
    let host = |w: u16| cx.site.param_slots.get(w as usize).is_some_and(|&s| s < 0);
    // memsig E-prog (C): a first walk only counts the computed keys and the parameters met (repeats included): most index
    // walks have none and stay another arm's, so the template's lists are made, in place and to size, only for ours
    let (mut n, mut m) = (0usize, 0usize);
    let Some(w) = computed_keys(keys, &host, &mut |_, _| n += 1, &mut |_| m += 1) else { return Ok(Ok(None)) };
    if n == 0 {
        return Ok(Ok(None));
    }
    if *iam != IndexAm::Btree {
        let leaf = match crate::ops::scan::sites_index_am::am_recipe(cx)? {
            Ok(r) if r.words.len() == w as usize => r,
            Ok(_) => return Ok(refuse("hosts", "a computed-key index recipe whose key words are not the graph's")),
            Err(why) => return Ok(refuse("hosts", why)),
        };
        return Ok(Ok(Some(template(cx, keys, &host, (n, m), leaf))));
    }
    let Some(r) = shapes::rte(cx.pstmt, *scanrelid as u32) else {
        return Ok(refuse("hosts", "the index scan's range table entry"));
    };
    let Some(indexqual) = plan_node(cx.pstmt, cx.node_id).and_then(|n| n.as_index_scan().map(|s| &s.indexqual).or_else(|| n.as_index_only_scan().map(|s| &s.indexqual))) else {
        return Ok(refuse("hosts", "the index scan's clauses"));
    };
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let index = match indexam::index_open(cx.mcx, *indexid as Oid, r.rellockmode) {
        Ok(i) => i,
        Err(e) => {
            heap.close(NoLock)?;
            return Err(e);
        }
    };
    let recipe = SiteRecipe::from_plan_words(&heap, &index, indexqual, r.rellockmode, cx.attnos).map(|r| r.with_backward(*backward).with_only(*only).with_row_budget(gravity_hosts::ops::scan::index_window::limit_budget(cx.pstmt, cx.node_id)).with_run_bound(gravity_hosts::ops::scan::index_window::run_bound(cx.pstmt, cx.node_id)));
    index.close(NoLock)?;
    heap.close(NoLock)?;
    let leaf = match recipe {
        Ok(r) => r,
        Err(e) => return Ok(refuse("hosts", e.message())),
    };
    if leaf.words.len() != w as usize {
        return Ok(refuse("hosts", "a computed-key index recipe whose key words are not the graph's"));
    }
    Ok(Ok(Some(template(cx, keys, &host, (n, m), leaf))))
}

/// memsig E-prog (C): walk `cx`'s template over recipe `leaf`, its lists built in place in the recipe set's root
/// (`AdmitCx::lists`): the computed keys' words (`n` of them, `admit`'s first walk), their parameters (each once; `m`
/// met, repeats included, bounds them) and `plan_keys`.
fn template(cx: &AdmitCx<'_>, keys: &[gravity_decompose::IndexKey<'_>], host: &dyn Fn(u16) -> bool, (n, m): (usize, usize), leaf: SiteRecipe) -> AdmitTpl {
    let (mut words, mut ids) = (cx.lists.list_with(n), cx.lists.list_with(m));
    // the keys `admit` counted, walked again: the same answer
    let _ = computed_keys(keys, host, &mut |w, _| cx.lists.push(&mut words, w), &mut |id| if !ids.contains(&id) { cx.lists.push(&mut ids, id) });
    let plan = plan_keys(cx, keys, host, n);
    RecipeTpl::Rung(Arc::new(Tpl { leaf, words, ids, node_id: cx.node_id, plan, leader: cx.leader })) // alloc: Program
}

/// A computed-key template into this execution's registry: each key compiled once over this execution's parameters.
pub(crate) fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> BindResult {
    let RecipeTpl::Rung(any) = t else { return Ok(Ok(false)) };
    let Some(t) = any.downcast_ref::<Tpl>() else { return Ok(Ok(false)) };
    // leader only (the host's `ExecVals` and compiled keys are thread-confined): a nest inner's or an index walk's pipeline
    // is serial; a parallel-eligible one runs on the leader when the program made it so (D3295), else is refused by name
    // D2754: the main registry on a front-key hit (no graph) reads what the admission recorded from the graph and the
    // plan (`Tpl::plan`); an InitPlan's or arm's registry (neither) is refused as before
    let parallel = match cx.graph {
        Some(g) => g.pipelines.iter().find_map(|p| walk_of(p, t.node_id)).map(|p| matches!(p.desc.parallel, Parallel::Eligible)),
        None => cx.front.and(t.plan.as_ref()).map(|k| k.parallel),
    };
    match parallel {
        Some(false) => {}
        Some(true) if t.leader => fires::fire("hosts.key.serial"),
        Some(true) => return Ok(refuse("hosts", "an index walk with a key computed from executor parameters in a parallel-eligible pipeline")),
        None => return Ok(refuse("hosts", "an index walk with a key computed from executor parameters outside the main graph")),
    }
    let vals = ExecVals::new(t.ids.iter().copied().max().unwrap_or(0).max(0) as usize + 1);
    let (ptr, n) = vals.bind();
    // the computed keys of this execution's graph (its nodes live for the execution): the admission's words and inputs
    // memsig E-prog (A): the keys in a list on this bind's owner, their parameters checked as walked (`SameIds`)
    let (mut computed, mut ids) = (cx.charged_vec(), SameIds::new(&t.ids));
    let found = match cx.graph {
        Some(g) => g.pipelines.iter().find_map(|p| walk_of(p, t.node_id)).and_then(|p| match &p.source {
            Source::Index { keys, .. } => computed_keys(keys, &|w| t.words.contains(&w), &mut |w, v| computed.with_vec(|l| l.push((w, v))), &mut |id| ids.meet(id)), // alloc: Exec charged cx.owner
            _ => None,
        }).is_some(),
        None => cx.front.zip(t.plan.as_ref()).is_some_and(|(pstmt, k)| plan_found(pstmt, t, k, &mut |w, v| computed.with_vec(|l| l.push((w, v))), &mut |id| ids.meet(id))), // alloc: Exec charged cx.owner
    };
    if !(found && ids.all() && computed.iter().map(|&(w, _)| w).eq(t.words.iter().copied())) {
        return Ok(refuse("hosts", "an index walk whose computed keys are not the admission's"));
    }
    let mut keys: Vec<(u16, KeyFn)> = Vec::with_capacity(computed.len()); // alloc: Program
    for &(w, v) in computed.iter() {
        let bind = ParamBind { exec_vals: Some(ptr), n_exec: n, ..shapes::param_bind(exprs) };
        let Some(st) = execexpr::exec_init_expr(cx.mcx, Some(v), bind)? else {
            return Ok(refuse("hosts", "an index key computed from executor parameters the executor did not initialize"));
        };
        let cell = core::cell::RefCell::new(st);
        let owner = std::thread::current().id();
        let f: KeyFn = Rc::new(move |mcx| {
            // D1356's rule for a compiled expression: thread-confined (the walk runs on the leader)
            debug_assert_eq!(std::thread::current().id(), owner, "gravity: a computed index key evaluated off its creating thread (D1016)");
            let mut st = cell.borrow_mut();
            st.arm_result_mcx(mcx);
            let nd = execexpr::exec_eval_expr(&mut *st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
            Ok((nd.value.as_u64(), nd.isnull))
        }); // alloc: Program
        keys.push((w, f)); // alloc: Program
    }
    // keys.rs rebinds the extern parameters of an Admitted that compiled an expression over them
    exprs.filters += 1;
    let base = t.leaf.words.len() as u16;
    let inputs: Vec<(u16, u16)> = t.ids.iter().enumerate().map(|(i, &id)| (base + i as u16, id as u16)).collect(); // alloc: Program
    reg.push_rung(Box::new(ParamKeyRecipe { leaf: t.leaf.clone(), keys: keys.into(), vals, inputs: inputs.into() })); // alloc: Program
    Ok(Ok(true))
}

/// The plan node numbered `id` in the statement (the main tree or a subplan), as m2_admit.rs `plan_node`.
fn plan_node(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, id: i32) -> Option<Node<'static>> {
    fn under(n: Option<Node<'static>>, id: i32) -> Option<Node<'static>> {
        stack_depth_core::check_stack_depth_or_panic();
        let n = n?;
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(n);
        }
        let kids = [n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|o| &o.bitmapplans), n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, id).or_else(|| under(p.righttree, id)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), id)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, id)))
    }
    under(pstmt.planTree, id).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, id)))
}

/// An index walk's computed keys in the host's word order (hosts `SiteRecipe::words`, lowerv4 source/index.rs, index_am.rs):
/// each one's word and expression to `out` (an `= ANY` array's, row 2; an AM or ORDER BY operand, row 4), the parameters
/// they read to `id` as the key walk meets them (lowerv4 params_key.rs `key_ids_by`: kept once each, in that order, they
/// are the input cells'), and the word count; `None`: an AM `= ANY`, a clause after ORDER BY (`out` may have had some).
/// D1630: a key bound at bind (params_key.rs `bound`) counts when `host` says its word is the host's (an inner walk's).
/// memsig E-prog: no list of its own; the template's are built in place in its set's root (`admit`), a bind's check is a
/// walk (`SameIds`).
fn computed_keys<'p>(keys: &[gravity_decompose::IndexKey<'p>], host: &dyn Fn(u16) -> bool, out: &mut dyn FnMut(u16, Node<'p>), id: &mut dyn FnMut(i32)) -> Option<u16> {
    let mut w = 0u16;
    for (i, k) in keys.iter().enumerate() {
        if k.form != KeyForm::OrderBy && keys[..i].iter().any(|o| o.form == KeyForm::OrderBy) {
            return None;
        }
        let (one, row) = match k.form {
            KeyForm::Op | KeyForm::Any | KeyForm::AmOp | KeyForm::OrderBy => (Some(k.value), None),
            KeyForm::Row => (None, k.value.as_row_compare_expr()),
            KeyForm::Null | KeyForm::NotNull => (None, None),
            KeyForm::AmAny => return None,
        };
        for v in one.into_iter().chain(row.into_iter().flat_map(|rc| rc.rargs.iter())) {
            if gravity_lowerv4::lifetime::bound::host_at(v, host(w)) {
                if !gravity_lowerv4::rerun::params_key::key_ids_by(v, id) {
                    return None;
                }
                out(w, v);
            }
            w += 1;
        }
    }
    Some(w)
}

/// memsig E-prog: a rung template's parameter list (`ids`: each executor parameter its computed values read, once, in the
/// key walk's order, lowerv4 params_key.rs `key_ids`) checked against the walk of this execution's values at bind, with
/// no list of the bind's own: `meet` each parameter the walk meets (`key_ids_by`), then `all`. Shared by param_tid.rs and param_sample.rs.
pub(crate) struct SameIds<'t> {
    ids: &'t [i32],
    seen: usize,
    same: bool,
}

impl<'t> SameIds<'t> {
    pub(crate) fn new(ids: &'t [i32]) -> SameIds<'t> {
        SameIds { ids, seen: 0, same: true }
    }

    /// The walk met `id`: while every parameter so far was the template's, those met are its first `seen`, so a new one
    /// must be its next.
    pub(crate) fn meet(&mut self, id: i32) {
        if self.same && !self.ids[..self.seen].contains(&id) {
            self.same = self.ids.get(self.seen) == Some(&id);
            self.seen += 1;
        }
    }

    /// The walk met exactly the template's parameters, in its order (`key_ids`' list equal to `ids`).
    pub(crate) fn all(&self) -> bool {
        self.same && self.seen == self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_decompose::IndexKey;
    use types_nodes::primnodes::{OpExpr, Param, ParamKind};
    use types_nodes::NodeList;

    /// D1016 row 4: an AM qual operand and an ORDER BY argument are one word each, the clause keys' first (lowerv4
    /// source/index_am.rs `key_words`); a clause key after an ORDER BY key (the 04:16Z review's F3) and an AM `= ANY` key
    /// fail closed (`None`); a null test has no word.
    #[test]
    fn am_and_order_by_keys_are_words_in_the_hosts_order() {
        let ctx = mcx::MemoryContext::new("d1016-row4");
        let mcx = ctx.mcx();
        let e = Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid: 3, paramtype: 600, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let c = Node::mk_const(mcx, 600, -1, 0, 16, datum::Datum::from_u64(0), true, false).unwrap();
        let add = Node::mk(mcx, OpExpr { opno: 731, opfuncid: 0, opresulttype: 600, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, e, c).unwrap(), location: -1 }).unwrap();
        let key = |form, value| IndexKey { attno: 1, col: 1, opno: 517, value, form };
        let words = |keys: &[IndexKey<'_>]| {
            let (mut o, mut ids) = (Vec::new(), Vec::new()); // alloc: Cold
            let w = computed_keys(keys, &|_| false, &mut |w, _| o.push(w), &mut |id| if !ids.contains(&id) { ids.push(id) }); // alloc: Cold
            w.map(|w| (o, ids, w))
        };
        assert_eq!(words(&[key(KeyForm::AmOp, c), key(KeyForm::OrderBy, add)]), Some((vec![1], vec![3], 2)), "a computed ORDER BY argument after a Const qual"); // alloc: Cold
        assert_eq!(words(&[key(KeyForm::AmOp, add), key(KeyForm::NotNull, c), key(KeyForm::OrderBy, c)]), Some((vec![0], vec![3], 2)), "a computed qual operand; a null test has no word"); // alloc: Cold
        assert_eq!(words(&[key(KeyForm::OrderBy, add), key(KeyForm::OrderBy, add)]), Some((vec![0, 1], vec![3], 2)), "two ORDER BY keys, one parameter"); // alloc: Cold
        assert_eq!(words(&[key(KeyForm::OrderBy, add), key(KeyForm::AmOp, c)]), None, "a clause key after an ORDER BY key");
        assert_eq!(words(&[key(KeyForm::AmAny, add)]), None, "an AM = ANY key");
        assert_eq!(words(&[key(KeyForm::AmOp, e), key(KeyForm::OrderBy, c)]), Some((vec![], vec![], 2)), "no computed key: M2's walk"); // alloc: Cold
    }

    /// The streaming check answers as `key_ids`' list compared with the template's: each parameter once, in order.
    #[test]
    fn same_ids_is_the_walks_list_equal_to_the_templates() {
        let walk = |ids: &[i32], met: &[i32]| {
            let mut s = SameIds::new(ids);
            met.iter().for_each(|&i| s.meet(i));
            s.all()
        };
        assert!(walk(&[3, 1], &[3, 1, 3, 1]), "repeats count once");
        assert!(walk(&[], &[]), "no parameter");
        assert!(!walk(&[3, 1], &[1, 3]), "another order");
        assert!(!walk(&[3, 1], &[3]), "one missing");
        assert!(!walk(&[3], &[3, 1]), "one more");
        assert!(!walk(&[3, 1], &[3, 2, 1]), "another between");
    }
}
