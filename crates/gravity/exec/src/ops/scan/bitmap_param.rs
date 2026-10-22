//! D1277 (M8-NEST Row A): BIND's hook for a one-index bitmap site whose recheck (`bitmapqualorig`) reads executor
//! parameters, `col = ANY ($n)` or `col op $n` over a nest's or SubPlan's outer value (hosts v4/m8_bitmap_param.rs). Every
//! parameter the qual reads must be a leaf key's bare argument, whose value the seek's key words carry (the lowerer's
//! EXEC_KEY cell, read at every re-open); the qual is compiled once here, over this execution's extern parameters and the
//! recipe's executor-parameter array, which the host's source writes from those words at every seek. Anything else keeps
//! m2_admit.rs's hosts refusal. W4a (V4R-550): `bind_tree`, the same for a BitmapAnd / BitmapOr tree (m2_admit.rs bind's
//! tree arm), its words every leaf's in leaf order (the site's). What this file newly serves (a tree; a one-index key
//! parameter the recheck does not read, D2185) keeps the bitmap no-widen line by name: no INTERSECT / EXCEPT (D2197) in
//! any pipeline or inner (a CTE, D2178 / D2179, left the line with their root fixes, see `hazard`; a hash or merge join,
//! D2194, left it at item 14 part 4: D2325 fixed, CAND-M39 not reproduced; a run-time key under a BitmapOr, D2180, left it
//! with its root fix 416c1b07a04: the host's tree seek empties a NULL-keyed leaf only, hosts v4/bitmap_window.rs
//! `Tree::seek`). D1016 row 6: a leaf key computed from executor parameters (lowerv4 params_key.rs
//! `computed_key`'s host cell, its parameters input cells after every leaf's words) compiled once per execution with
//! ExecInitExpr over the same array, evaluated by the host at every seek before the bitmap build (ExecReScanBitmapIndexScan's
//! ExecIndexEvalRuntimeKeys); such a site's statement keeps the lowerer's no-widen line (lowerv4 source/bitmap.rs), not
//! `hazard`'s. Created by M8-NEST.
use std::rc::Rc;

use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::graph::{IndexKey, KeyForm};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, PipelineGraph, PipelineNode, Source};
use gravity_hosts::ops::scan::bitmap_window::TreeRecipe;
use gravity_hosts::ops::scan::bitmap_param::{ExecVals, ParamBitmapRecipe, ParamTreeRecipe};
use gravity_hosts::ops::shared::key_fn::KeyFn;
use gravity_hosts::{FilterFn, HostRegistry};
use types_error::PgResult;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;
use types_portal::params::ParamBind;

use crate::admit::{refuse, sources, BindCx, RecipeTpl, Refused};
use crate::shapes;

const LEAF_WORDS: &str = "a bitmap tree whose key words are not its leaves'";
/// D1016 row 6: a one-index site's key words not its recipe's (the host's derive would assert).
const INDEX_WORDS: &str = "a bitmap index scan whose key words are not its recipe's (D1016 row 6)";
/// main 01:52Z / 02:22Z (fail closed, as D1016 row 2 and D652): an INTERSECT / EXCEPT read through an InitPlan loses its
/// second input (D2197).
const SETOP: &str = "a bitmap recheck over an executor parameter in a statement with a set operation (an INTERSECT / EXCEPT read through an InitPlan loses its second input: D2197)";

/// `Some` when `t` is a one-index bitmap whose recheck reads executor parameters: bound into `reg` (`Ok(true)`) or refused
/// by name. `None`: m2_admit.rs's bind takes `t` as before.
pub(crate) fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Option<Result<bool, Refused>>> {
    let RecipeTpl::Bitmap(r, relid, indexid, node, heap, words) = t else { return Ok(None) };
    let Some(graph) = cx.graph else { return front_bind((r, *relid, *node), (*heap, words.as_deref()), reg, cx, exprs) };
    let mut all = cx.charged_vec();
    all.with_vec(|v| graph.pipelines.iter().for_each(|p| sources(p, v))); // alloc: Exec charged cx.owner
    let same = || {
        all.iter().filter_map(|(s, _)| match s {
            Source::Bitmap { relid: rr, plan: p @ BitmapPlan::Index { indexid: i, keys, .. }, recheck, .. } if rr == relid && i == indexid => Some((p, keys, *recheck, is_inner(graph, s))),
            _ => None,
        })
    };
    // D2207: the site's own scan, by its node id (another scan over the index has its own keys and recheck)
    let Some((_, keys, q, inner)) = same().find(|(p, ..)| matches!(p, BitmapPlan::Index { node_id, .. } if node_id == node)) else { return Ok(None) };
    let mut read = cx.charged_vec();
    exec_ids(q, &mut read);
    let mut at = Words::on(cx);
    if !key_words(keys, inner, &mut at) {
        return Ok(Some(refuse("hosts", WALK)));
    }
    if read.is_empty() && at.keys.is_empty() {
        return Ok(None);
    }
    if usize::from(at.w) != r.words_len() {
        return Ok(Some(refuse("hosts", INDEX_WORDS)));
    }
    if let Some(why) = hazard(graph).filter(|_| at.keys.is_empty() && at.words.iter().any(|&(_, p)| !read.contains(&i32::from(p)))) {
        return Ok(Some(refuse("hosts", why)));
    }
    at.inputs();
    push_leaf(r, q, parts(q, &read, at, cx, exprs)?, reg, cx, exprs)
}

/// The bound one-index recipe into `reg`, its leaf with the BitmapHeapScan qual's pre-check (D2988, sites.rs
/// `bitmap_prefilter`), or `parts`' refusal.
#[allow(clippy::type_complexity)]
fn push_leaf(r: &crate::admit::SiteTpl, q: &types_nodes::NodeList<'static>, parts: Result<(FilterFn, Rc<ExecVals>, Rc<[(u16, u16)]>, Rc<[(u16, KeyFn)]>), Refused>, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Option<Result<bool, Refused>>> {
    let (recheck, vals, words, keys) = match parts {
        Ok(p) => p,
        Err(why) => return Ok(Some(Err(why))),
    };
    let leaf = crate::ops::scan::sites::bitmap_prefilter(r.bind_copy(), q, cx, exprs)?;
    reg.push_rung(Box::new(ParamBitmapRecipe { leaf, recheck, vals, words, keys })); // alloc: Program
    Ok(Some(Ok(true)))
}

/// D2754 part 4: what a front-key hit, bound without the graph, needs to bind a one-index bitmap whose recheck reads
/// executor parameters as `bind` does: the key words in the host's order (word, parameter id) and the parameters the recheck
/// reads, as the admitting graph's `bind` found them. The front key keys every Param's kind and id (inputs/front.rs), so
/// every statement filed under one key has these ids at these words.
pub struct ParamWords {
    words: Box<[(u16, u16)]>,
    read: Box<[i32]>,
}

/// D2754 part 4: `ParamWords` for bitmap source `s` of `graph` when `bind` would bind it with a parameter-reading recheck and
/// no key the host computes (`key_words`' bare PARAM_EXEC arguments only; a computed key stays graph-bound), a recipe of
/// `n_words` words, no hazard line crossed, and every parameter the recheck reads a key's; `None`: graph-bound as before.
pub(crate) fn front_words(graph: &PipelineGraph<'_>, s: &Source<'_>, n_words: usize) -> Option<ParamWords> {
    let Source::Bitmap { plan: BitmapPlan::Index { keys, .. }, recheck, .. } = s else { return None };
    // memsig E-prog: the admission's scratch on the admitting statement (the recipe set's owner at birth, admit `registry`)
    fn on<T>() -> memledger::ChargedVec<T> {
        memledger::ChargedVec::new(Some(memledger::charge::statement_owner()), memledger::Kind::Region) // alloc: Exec charged statement_owner
    }
    let mut read = on();
    exec_ids(recheck, &mut read);
    let mut at = Words { words: on(), keys: on(), ids: on(), w: 0 };
    if read.is_empty() || !key_words(keys, is_inner(graph, s), &mut at) || !at.keys.is_empty() || usize::from(at.w) != n_words {
        return None;
    }
    if hazard(graph).is_some() && at.words.iter().any(|&(_, p)| !read.contains(&i32::from(p))) {
        return None;
    }
    if read.iter().any(|id| !at.words.iter().any(|&(_, p)| i32::from(p) == *id)) {
        return None;
    }
    at.inputs();
    Some(ParamWords { words: at.words[..].into(), read: read[..].into() }) // alloc: Program
}

/// D2754 part 4: `bind` on a front-key hit (no graph): a one-index bitmap whose admission recorded `ParamWords` and the
/// BitmapHeapScan's node id compiles this execution's own recheck, that node's `bitmapqualorig` (sites_heap.rs
/// `front_qual`), over the recorded words, as `bind` compiles the graph's. A recheck reading other parameters refuses by
/// name. `None` (no words recorded, or no front plan: an InitPlan's or an Append arm's registry): m2_admit.rs's bind as before.
fn front_bind(site: (&crate::admit::SiteTpl, u32, i32), front: (Option<i32>, Option<&ParamWords>), reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Option<Result<bool, Refused>>> {
    let ((r, relid, node), (Some(pw), Some((pstmt, id)))) = (site, (front.1, cx.front.zip(front.0))) else { return Ok(None) };
    let Some(q) = crate::ops::scan::sites_heap::front_qual(pstmt, id, relid, node) else {
        return Ok(Some(refuse("hosts", "a bitmap recipe without its source in this execution's plan")));
    };
    if !same_exec_ids(q, &pw.read) {
        return Ok(Some(refuse("hosts", "a bitmap recheck over executor parameters other than its filed key words' (D2754)")));
    }
    let mut at = Words::on(cx);
    at.words.with_vec(|v| v.extend_from_slice(&pw.words)); // alloc: Exec charged cx.owner
    push_leaf(r, q, parts(q, &pw.read, at, cx, exprs)?, reg, cx, exprs)
}

/// W4a: `Some` when the tree `tree` (over `relid`, plan root `root`) has a recheck reading executor parameters: bound into
/// `reg` or refused by name. `None`: m2_admit.rs's tree arm takes it as before.
pub(crate) fn bind_tree(tree: &TreeRecipe, relid: u32, root: i32, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Option<Result<bool, Refused>>> {
    let Some(graph) = cx.graph else { return Ok(None) };
    let mut all = cx.charged_vec();
    all.with_vec(|v| graph.pipelines.iter().for_each(|p| sources(p, v))); // alloc: Exec charged cx.owner
    let Some((plan, q, inner)) = all.iter().find_map(|(s, _)| match s {
        Source::Bitmap { relid: r, plan, recheck, .. } if *r == relid && matches!(plan, BitmapPlan::And { node_id, .. } | BitmapPlan::Or { node_id, .. } if *node_id == root) => Some((plan, *recheck, is_inner(graph, s))),
        _ => None,
    }) else {
        return Ok(None);
    };
    let mut read = cx.charged_vec();
    exec_ids(q, &mut read);
    let computed = computed_in(plan, inner);
    if read.is_empty() && !computed {
        return Ok(None);
    }
    let mut lens = cx.charged_vec();
    lens.with_vec(|v| v.extend(tree.leaves.iter().map(|l| l.words.len()))); // alloc: Exec charged cx.owner
    // D1016 row 6: a tree with a computed key keeps the lowerer's no-widen line (D2180 and D2197 left main's list)
    let at = match hazard(graph).filter(|_| !computed).map_or_else(|| site_words(plan, inner, &lens, Words::on(cx)), Err) {
        Ok(at) => at,
        Err(why) => return Ok(Some(refuse("hosts", why))),
    };
    Ok(Some(parts(q, &read, at, cx, exprs)?.map(|(recheck, vals, words, keys)| {
        reg.push_rung(Box::new(ParamTreeRecipe { tree: tree.clone(), recheck, vals, words, keys })); // alloc: Program
        true
    })))
}

/// A key's arguments, one per key word: a comparison's or `= ANY`'s one, a row comparison's members, a null test's none.
/// memsig E-prog: walked in place, no list (as param_key.rs `computed_keys`).
fn args<'a, 'p>(k: &'a IndexKey<'p>) -> impl Iterator<Item = Node<'p>> + 'a {
    let (one, row) = match k.form {
        KeyForm::Row => (None, k.value.as_row_compare_expr()),
        KeyForm::Null | KeyForm::NotNull | KeyForm::OrderBy => (None, None),
        _ => (Some(k.value), None),
    };
    one.into_iter().chain(row.into_iter().flat_map(|rc| rc.rargs.iter()))
}

/// A bitmap site's words from its keys: `(word, paramid)` for a bare PARAM_EXEC argument's word, then (`inputs`) each input
/// cell's after every leaf's words; D1016 row 6's computed keys `(word, expression)` and the parameters they read in lowerv4
/// params_key.rs `key_ids` order (the input cells'); `w` the next word.
/// memsig E-prog (A): a bind's lists are on its owner (`on`, `BindCx::charged_vec`): scratch the bind turns into the
/// recipe's own (`parts`) and drops.
#[derive(Default)]
struct Words<'p> {
    words: memledger::ChargedVec<(u16, u16)>,
    keys: memledger::ChargedVec<(u16, Node<'p>)>,
    ids: memledger::ChargedVec<i32>,
    w: u16,
}

impl<'p> Words<'p> {
    /// Empty lists on bind `cx`'s owner.
    fn on(cx: &BindCx<'_>) -> Words<'p> {
        Words { words: cx.charged_vec(), keys: cx.charged_vec(), ids: cx.charged_vec(), w: 0 }
    }

    /// The input cells, one per parameter the computed keys read, after the leaves' words (lowerv4 source/bitmap.rs).
    fn inputs(&mut self) {
        let (base, ids) = (self.w, &self.ids);
        self.words.with_vec(|v| v.extend(ids.iter().enumerate().map(|(i, &id)| (base + i as u16, id as u16)))); // alloc: Exec charged self.words
    }
}

/// A computed key's walk met a node the key walk does not know (the lowerer refuses it first: params_key.rs `computed_key`).
const WALK: &str = "a bitmap index key computed from executor parameters over a node the key walk does not know (D1016 row 6)";

/// Whether source `s` is a nest's or SubPlan's inner walk (under no pipeline of `graph` directly; lowerv4 params_key.rs
/// `per_open`'s pipeline past the graph's).
/// D2754: whether `bind` / `bind_tree` leave bitmap source `s` of `graph` to the plain recheck (they answer `None`): its
/// recheck reads no executor parameter and no leaf key is the host's (`computed_in`). A front-key hit, bound without the
/// graph, binds only such a recheck from its own plan (sites_heap.rs `plan_recheck`).
pub(crate) fn plain(graph: &PipelineGraph<'_>, s: &Source<'_>) -> bool {
    let Source::Bitmap { plan, recheck, .. } = s else { return false };
    !reads_exec(recheck) && !computed_in(plan, is_inner(graph, s))
}

fn is_inner(graph: &PipelineGraph<'_>, s: &Source<'_>) -> bool {
    !graph.pipelines.iter().any(|p| std::ptr::addr_eq(&p.source, s))
}

/// D1016 row 6 / D1630: whether key argument `a` is the host's at every re-open: computed from executor parameters, or bound
/// at bind on an inner walk (lowerv4 lifetime/bound.rs `host_at`).
fn host(a: Node<'_>, inner: bool) -> bool {
    gravity_lowerv4::lifetime::bound::host_at(a, inner)
}

/// Each key's words in the host's order (index_source.rs `build`, lowerv4 source/bitmap.rs `leaf_words`) from `at.w` on; a
/// bare PARAM_EXEC argument's word is its value; a host argument's (`host`) the host's (D1016 row 6, D1630). `false`: `WALK`.
fn key_words<'p>(keys: &[IndexKey<'p>], inner: bool, at: &mut Words<'p>) -> bool {
    for a in keys.iter().flat_map(|k| args(k)) {
        let w = at.w;
        if let Some(p) = a.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC && p.paramid >= 0) {
            at.words.with_vec(|v| v.push((w, p.paramid as u16))); // alloc: Exec charged at.words
        } else if host(a, inner) {
            let ids = &mut at.ids;
            if !gravity_lowerv4::rerun::params_key::key_ids_by(a, &mut |id| if !ids.contains(&id) { ids.with_vec(|v| v.push(id)) }) { // alloc: Exec charged at.ids
                return false;
            }
            at.keys.with_vec(|v| v.push((w, a))); // alloc: Exec charged at.keys
        }
        at.w += 1;
    }
    true
}

/// D1016 row 6 / D1630: whether a leaf of `plan` has a host key (`host`).
fn computed_in(plan: &BitmapPlan<'_>, inner: bool) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    match plan {
        BitmapPlan::Index { keys, .. } => keys.iter().flat_map(|k| args(k)).any(|a| host(a, inner)),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().any(|p| computed_in(p, inner)),
    }
}

/// The tree's words, its leaves' in leaf order (m2_admit.rs `leaf_recipes`, lowerv4 source/bitmap_tree.rs `tree`), each
/// leaf's count its recipe's (`lens`), then the input cells, into `at` (empty: `Words::on`); `Err` a refusal's name.
fn site_words<'p>(plan: &BitmapPlan<'p>, inner: bool, lens: &[usize], at: Words<'p>) -> Result<Words<'p>, &'static str> {
    fn walk<'p>(plan: &BitmapPlan<'p>, inner: bool, lens: &[usize], leaf: &mut usize, at: &mut Words<'p>) -> Result<(), &'static str> {
        stack_depth_core::check_stack_depth_or_panic();
        match plan {
            BitmapPlan::Index { keys, .. } => {
                let w0 = at.w;
                if !key_words(keys, inner, at) {
                    return Err(WALK);
                }
                *leaf += 1;
                if lens.get(*leaf - 1) == Some(&usize::from(at.w - w0)) { Ok(()) } else { Err(LEAF_WORDS) }
            }
            BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().try_for_each(|p| walk(p, inner, lens, leaf, at)),
        }
    }
    let (mut leaf, mut at) = (0, at);
    walk(plan, inner, lens, &mut leaf, &mut at)?;
    at.inputs();
    if leaf == lens.len() { Ok(at) } else { Err(LEAF_WORDS) }
}

/// The bitmap no-widen line over the whole statement: in any op of any pipeline or inner an INTERSECT / EXCEPT
/// (`SetDesc::Set` / `Identity`, D2197). A recursive union's iteration (`SetDesc::Iter`) is not one. A CTE (D2178 / D2179:
/// a reader stopping it early and a later one resuming it re-read rows; a data-modifying one read by a stopping reader
/// wrote again) left the line with their root fixes: a CTE's store is filled and parked only by its readers' pulls, so a
/// stopping reader and a later one read it as PostgreSQL's CTE Scans do. A run-time key under a BitmapOr (D2180) left the
/// line with its root fix (416c1b07a04): the host's tree seek makes a NULL-keyed leaf an empty bitmap, so a BitmapOr unions
/// its other children as MultiExecBitmapOr does.
fn hazard(graph: &PipelineGraph<'_>) -> Option<&'static str> {
    let setop = graph.pipelines.iter().any(|p| any_op(p, &|o| matches!(o, Op::SetOp(SetDesc::Set { .. } | SetDesc::Identity { .. }))));
    setop.then_some(SETOP)
}

/// `f` holds for an op of `p` or of a nest or nested SubPlan inner it embeds (admit.rs `sources`' walk).
fn any_op(p: &PipelineNode<'_>, f: &dyn Fn(&Op<'_>) -> bool) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    p.ops.iter().any(|o| {
        f(o) || match o {
            Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => any_op(inner, f),
            _ => false,
        }
    })
}

/// The recheck `q` compiled over an executor-parameter array the seek writes (every key word's parameter and every one
/// `read`); refused by name when it reads a parameter no key word carries or calls a volatile function.
fn compiled(q: &types_nodes::NodeList<'static>, read: &[i32], words: &[(u16, u16)], cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Result<(FilterFn, Rc<ExecVals>), Refused>> {
    if read.iter().any(|id| !words.iter().any(|&(_, p)| p as i32 == *id)) {
        return Ok(refuse("hosts", "a bitmap recheck over an executor parameter that is not an index key's argument"));
    }
    // a key's parameter the recheck does not read (PostgreSQL drops a lossy-derived clause from bitmapqualorig) is written too
    let vals = ExecVals::new(words.iter().map(|&(_, p)| p as usize).max().unwrap_or(0) + 1);
    let (ptr, n) = vals.bind();
    let bind = ParamBind { exec_vals: Some(ptr), n_exec: n, ..shapes::param_bind(exprs) };
    let Some(Some(f)) = shapes::compile_filter_bound(cx.mcx, q, bind)? else {
        return Ok(refuse("hosts", "a bitmap recheck qual the host cannot compile (a volatile function)"));
    };
    // keys.rs rebinds the extern parameters of an Admitted that compiled a filter
    exprs.filters += 1;
    Ok(Ok((f, vals)))
}

/// The recheck compiled over the site's parameter array (`compiled`) and D1016 row 6's computed keys over the same array
/// (`key_fn`); a key the executor cannot initialize refuses by name.
#[allow(clippy::type_complexity)]
fn parts(q: &types_nodes::NodeList<'static>, read: &[i32], at: Words<'static>, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Result<(FilterFn, Rc<ExecVals>, Rc<[(u16, u16)]>, Rc<[(u16, KeyFn)]>), Refused>> {
    let (recheck, vals) = match compiled(q, read, &at.words, cx, exprs)? {
        Ok(c) => c,
        Err(why) => return Ok(Err(why)),
    };
    let mut keys = Vec::with_capacity(at.keys.len()); // alloc: Program
    for &(w, v) in at.keys.iter() {
        let Some(f) = key_fn(v, &vals, cx, exprs)? else {
            return Ok(refuse("hosts", "a bitmap index key computed from executor parameters the executor did not initialize (D1016 row 6)"));
        };
        keys.push((w, f)); // alloc: Program
    }
    Ok(Ok((recheck, vals, at.words[..].into(), keys.into())))
}

/// D1016 row 6: computed key `v` compiled once per execution over the site's parameter array (as exec v4/m8_param_key.rs
/// bind's), which the host evaluates at every seek: ExecReScanBitmapIndexScan's ExecIndexEvalRuntimeKeys.
fn key_fn(v: Node<'static>, vals: &ExecVals, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> PgResult<Option<KeyFn>> {
    let (ptr, n) = vals.bind();
    let bind = ParamBind { exec_vals: Some(ptr), n_exec: n, ..shapes::param_bind(exprs) };
    let Some(st) = execexpr::exec_init_expr(cx.mcx, Some(v), bind)? else { return Ok(None) };
    let (cell, owner) = (core::cell::RefCell::new(st), std::thread::current().id());
    Ok(Some(Rc::new(move |mcx| {
        // D1356's rule for a compiled expression: thread-confined (a bitmap site runs on the leader)
        debug_assert_eq!(std::thread::current().id(), owner, "gravity: a computed bitmap key evaluated off its creating thread (D1016 row 6)");
        let mut st = cell.borrow_mut();
        st.arm_result_mcx(mcx);
        let nd = execexpr::exec_eval_expr(&mut *st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
        Ok((nd.value.as_u64(), nd.isnull))
    }))) // alloc: Program
}

/// The PARAM_EXEC ids the recheck `q` reads, each once, into `out` (a bind's on its owner: `BindCx::charged_vec`).
fn exec_ids(q: &types_nodes::NodeList<'_>, out: &mut memledger::ChargedVec<i32>) {
    each_exec_id(q, &mut |id| if !out.contains(&id) { out.with_vec(|v| v.push(id)) }); // alloc: Exec charged out
}

/// Each PARAM_EXEC id the recheck `q` reads to `f`, as the walk meets it (repeats included).
fn each_exec_id(q: &types_nodes::NodeList<'_>, f: &mut dyn FnMut(i32)) {
    q.iter().for_each(|n| {
        gravity_decompose::expr::walk(n, &mut |x| {
            if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
                f(p.paramid);
            }
            true
        });
    });
}

/// The recheck `q` reads some PARAM_EXEC (`exec_ids` not empty), with no list.
fn reads_exec(q: &types_nodes::NodeList<'_>) -> bool {
    let mut any = false;
    each_exec_id(q, &mut |_| any = true);
    any
}

/// The recheck `q` reads exactly the PARAM_EXEC ids `ids`, each once in walk order (`exec_ids` equal to `ids`), with no list
/// (param_key.rs `SameIds`).
fn same_exec_ids(q: &types_nodes::NodeList<'_>, ids: &[i32]) -> bool {
    let mut same = crate::ops::scan::param_key::SameIds::new(ids);
    each_exec_id(q, &mut |id| same.meet(id));
    same.all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_decompose::IndexAm;
    use gravity_program::{Arena, ArenaRef, ArenaVec};
    use types_nodes::primnodes::{OpExpr, Param, RowCompareExpr};
    use types_nodes::NodeList;

    /// W4a (V4R-550): the tree's words in leaf order, a leaf's word count its recipe's; a key under a BitmapOr other than a
    /// non-null constant (an executor or extern parameter, a NULL) takes its words as any leaf's (D2180 left main's no-widen
    /// line with its root fix: the host's seek empties a NULL-keyed leaf only).
    #[test]
    fn tree_words_in_leaf_order() {
        let ctx = mcx::MemoryContext::new("v4r-550");
        let mcx = ctx.mcx();
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let param = |kind, id| Node::mk(mcx, Param { paramkind: kind, paramid: id, paramtype: types_core::INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let (x0, x1, x2, e1) = (param(ParamKind::PARAM_EXEC, 0), param(ParamKind::PARAM_EXEC, 1), param(ParamKind::PARAM_EXEC, 2), param(ParamKind::PARAM_EXTERN, 1));
        let c = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(123), false, true).unwrap();
        let n = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(0), true, true).unwrap();
        let row = Node::mk(mcx, RowCompareExpr { rargs: NodeList::from_slice(mcx, &[x1, x2]).unwrap(), ..Default::default() }).unwrap();
        fn leaf<'p>(a: ArenaRef, ks: &[(KeyForm, Node<'p>)]) -> BitmapPlan<'p> {
            let mut keys = ArenaVec::new_in(a);
            ks.iter().for_each(|&(form, value)| keys.push(IndexKey { attno: 1, col: 1, opno: 96, value, form }));
            BitmapPlan::Index { node_id: 1, indexid: 1, keys, rows: 1, iam: IndexAm::Btree }
        }
        fn node<'p>(a: ArenaRef, and: bool, kids: Vec<BitmapPlan<'p>>) -> BitmapPlan<'p> {
            let mut plans = ArenaVec::new_in(a);
            kids.into_iter().for_each(|p| plans.push(p));
            if and { BitmapPlan::And { node_id: 0, plans } } else { BitmapPlan::Or { node_id: 0, plans } }
        }
        let words = |t: &BitmapPlan<'_>, lens: &[usize]| site_words(t, false, lens, Words::default()).map(|at| at.words.to_vec()); // alloc: Cold
        let t = node(a, true, vec![leaf(a, &[(KeyForm::Op, x0)]), leaf(a, &[(KeyForm::Op, c)])]); // alloc: Cold
        assert_eq!(words(&t, &[1, 1]), Ok(vec![(0, 0)]), "partition_join#258: BitmapAnd(b = $0, c = 123)"); // alloc: Cold
        assert_eq!(words(&t, &[1, 2]), Err(LEAF_WORDS), "a leaf's words are not its recipe's");
        assert_eq!(words(&t, &[1, 1, 0]), Err(LEAF_WORDS), "a leaf the plan does not have");
        assert!(!computed_in(&t, false), "bare parameters and constants are no computed key");
        let t = node(a, true, vec![leaf(a, &[(KeyForm::NotNull, c)]), leaf(a, &[(KeyForm::Row, row), (KeyForm::Any, x0)]), node(a, false, vec![leaf(a, &[(KeyForm::Op, c)]), leaf(a, &[(KeyForm::Op, c)])])]); // alloc: Cold
        assert_eq!(words(&t, &[0, 3, 1, 1]), Ok(vec![(0, 1), (1, 2), (2, 0)]), "a null test none, a row comparison one per member, a constant Or"); // alloc: Cold
        let t = node(a, true, vec![leaf(a, &[(KeyForm::Op, x0)]), node(a, false, vec![leaf(a, &[(KeyForm::Op, e1)]), leaf(a, &[(KeyForm::Op, c)])])]); // alloc: Cold
        assert_eq!(words(&t, &[1, 1, 1]), Ok(vec![(0, 0)]), "an extern parameter under a BitmapOr beside an executor one (D2180)"); // alloc: Cold
        let t = node(a, false, vec![node(a, true, vec![leaf(a, &[(KeyForm::Op, x0)]), leaf(a, &[(KeyForm::Op, c)])]), leaf(a, &[(KeyForm::Op, c)])]); // alloc: Cold
        assert_eq!(words(&t, &[1, 1, 1]), Ok(vec![(0, 0)]), "an executor parameter under an And under a BitmapOr"); // alloc: Cold
        let t = node(a, true, vec![leaf(a, &[(KeyForm::Op, c)]), node(a, false, vec![leaf(a, &[(KeyForm::Row, row)]), leaf(a, &[(KeyForm::Op, c)])])]); // alloc: Cold
        assert_eq!(words(&t, &[1, 2, 1]), Ok(vec![(1, 1), (2, 2)]), "a row comparison's parameter members under a BitmapOr"); // alloc: Cold
        let t = node(a, false, vec![leaf(a, &[(KeyForm::Op, n)]), leaf(a, &[(KeyForm::Op, c)])]); // alloc: Cold
        assert_eq!(words(&t, &[1, 1]), Ok(vec![]), "a NULL constant key under a BitmapOr: no word, its leaf empty at the seek"); // alloc: Cold
        // D1016 row 6: partition_join#259 BitmapOr(b = $1 + 1, c = 99) and a computed key beside a bare one reading $2 and $1
        let op = |l, r| Node::mk(mcx, OpExpr { opno: 551, opfuncid: 177, opresulttype: types_core::INT4OID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, l, r).unwrap(), location: -1 }).unwrap();
        let t = node(a, false, vec![leaf(a, &[(KeyForm::Op, op(x1, c))]), leaf(a, &[(KeyForm::Op, c)])]); // alloc: Cold
        assert!(computed_in(&t, false), "a computed key under a BitmapOr");
        let at = site_words(&t, false, &[1, 1], Words::default()).expect("leaf words"); // alloc: Cold
        assert_eq!((at.words.to_vec(), at.keys.iter().map(|k| k.0).collect::<Vec<_>>(), at.ids.to_vec()), (vec![(2, 1)], vec![0], vec![1]), "the key's word 0, its parameter's input cell after the leaves' two words"); // alloc: Cold
        let t = node(a, true, vec![leaf(a, &[(KeyForm::Op, x2), (KeyForm::Op, op(x2, x1))]), leaf(a, &[(KeyForm::Any, x0)])]); // alloc: Cold
        let at = site_words(&t, false, &[2, 1], Words::default()).expect("leaf words"); // alloc: Cold
        assert_eq!((at.words.to_vec(), at.keys.iter().map(|k| k.0).collect::<Vec<_>>(), at.ids.to_vec()), (vec![(0, 2), (2, 0), (3, 2), (4, 1)], vec![1], vec![2, 1]), "bare words first, then the input cells in key_ids order"); // alloc: Cold
        // D1630: a key over no executor parameter ($1 + 123) is the host's on an inner walk only, with no input cell
        let t = node(a, true, vec![leaf(a, &[(KeyForm::Op, op(e1, c))]), leaf(a, &[(KeyForm::Op, x0)])]); // alloc: Cold
        assert!(!computed_in(&t, false) && computed_in(&t, true), "a bound key is a host key on an inner walk only");
        let at = site_words(&t, true, &[1, 1], Words::default()).expect("leaf words"); // alloc: Cold
        assert_eq!((at.words.to_vec(), at.keys.iter().map(|k| k.0).collect::<Vec<_>>(), at.ids.to_vec()), (vec![(1, 0)], vec![0], vec![]), "the bound key's word 0, no input cell"); // alloc: Cold
    }
}
