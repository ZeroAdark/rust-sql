//! M2's site admissions (cards/M2.md L3, V4R-25 / Decision 5): the host recipe of a heap range site (`src.heap`), a bitmap
//! site (one index, or a BitmapAnd / BitmapOr tree), a TID site (points, range, `WHERE CURRENT OF`) and a TABLESAMPLE site,
//! ported from lowerv3-final's exec/src/admit/v3_r1.rs / v3_r1b.rs (the M1 pattern: m1_admit.rs). The btree index and
//! index-only sites stay M1's arm; an any-AM index site waits on M1's arm (it refuses a non-btree first). A source kind
//! the lowerer does not serve never reaches admission. Created by M2 (owner M2-HOSTS); m2.rs re-exports its tables.
use std::sync::Arc;

use gravity_decompose::catalog::scan::{tid_op, TidOp};
use gravity_decompose::ops::shared::scan::{BitmapPlan, TidForm};
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{Am, IndexAm, IndexKey, Source};
use gravity_hosts::ops::scan::am_rows::AmRowsRecipe;
use gravity_hosts::ops::scan::bitmap_morsel::{BitmapMorselRecipe, MorselBitmap};
use gravity_hosts::ops::scan::bitmap_window::{BitmapTreeRecipe, Shape, TreeRecipe};
use gravity_hosts::ops::scan::tid_sample::{SampleRecipe, TidClause, TidRecipe};
use gravity_hosts::{HostRegistry, RangeRecipe, SiteRecipe};
use gravity_program::SiteDesc;
use types_core::Oid;
use types_nodes::{Node, NodeList};
use types_storage::lock::NoLock;

use crate::admit::{refuse, sources, Admit, AdmitCx, AdmitResult, Bind, BindCx, BindResult, RecipeTpl, Refused};
use crate::shapes;

pub const ADMIT: &[Admit] = &[range, bitmap, tid, crate::ops::scan::param_tid::admit, sample, index_array, crate::ops::scan::sites_index_am::any_am];
pub const BIND: &[Bind] = &[bind, crate::ops::scan::param_tid::bind];

/// A bitmap tree's recipe as filed under the key: the tree, and the heap relation plus the tree root's plan node, by
/// which BIND finds this execution's `bitmapqualorig` in the graph; `heap` (D2754): the BitmapHeapScan's plan node, from
/// which a front-key hit reads it instead (`plan_recheck`).
pub(crate) struct TreeTpl {
    pub(crate) tree: TreeRecipe,
    pub(crate) relid: u32,
    pub(crate) root: i32,
    pub(crate) heap: Option<i32>,
}

/// A recipe template holding a relation's cached descriptors or compiled quals (not `Send` / `Sync`) as a `RecipeTpl::Rung`.
pub(crate) struct Local<T>(pub(crate) T);
// Send / Sync: ops/scan/raw.rs (a recipe template never crosses threads).

/// A rows-road site's template (PGRCSERVE G4; D4735: a sample's too), its recipe's error as the hosts refusal.
fn rows_tpl(recipe: types_error::PgResult<AmRowsRecipe>) -> AdmitResult {
    Ok(recipe.map(|r| Some(RecipeTpl::Rung(Arc::new(r)))).or_else(|e| refuse("hosts", e.message()))) // alloc: Program
}

fn range(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Range { am: Am::Heap, .. } = cx.source else { return Ok(Ok(None)) };
    if cx.site.lanes().iter().any(|l| l.id == gravity_program::LaneId::RowId) {
        return Ok(Ok(None)); // a by-rowid fetch site (ops/sort/fetch.rs): a scan site never keeps its RowId lane
    }
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    if cx.site.rows_via_am {
        let recipe = AmRowsRecipe::from_plan(&heap, cx.attnos, &lane_words(cx.site, gravity_abi::ctx::lane::KIND_BYREF)).map(|r| r.at_node(cx.node_id));
        heap.close(NoLock)?;
        return rows_tpl(recipe);
    }
    let recipe = RangeRecipe::from_plan_byref(&heap, cx.attnos, &lane_words(cx.site, gravity_abi::ctx::lane::KIND_BYREF));
    heap.close(NoLock)?;
    // D3585: a relation-set site reads its twins by the first relation's recipe (each execution resolves their storage)
    let set = (cx.site.set.len() > 1).then(|| cx.site.set.iter().map(|&r| r as Oid).collect::<Arc<[Oid]>>()); // alloc: Program
    let recipe = recipe.map(|mut r| {
        r.set_pruned = set.is_some() && cx.site.set_prune.is_some(); // D3663
        r.set = set;
        (r.run_counted, r.seek_counted) = (cx.run_counted, cx.probed); // D3670
        r
    });
    Ok(recipe.map(|r| Some(RecipeTpl::Range(r))).or_else(|e| refuse("hosts", e.message())))
}

/// The morsel form's template (decompose closure.rs `morsel_form`): the heap range recipe its walk
/// deforms by and the bitmap the runner builds (hosts bitmap_morsel.rs); BIND files a fresh recipe per execution.
pub(crate) struct MorselTpl {
    pub(crate) range: RangeRecipe,
    pub(crate) bitmap: MorselBitmap,
}

fn bitmap(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Bitmap { plan, scanrelid, morsel, .. } = cx.source else { return Ok(Ok(None)) };
    // the lowerer took the morsel form only where the site stayed the decomposer's range site (lowerv4 bitmap.rs)
    let morsel = *morsel && cx.site.is_range();
    let Some(r) = shapes::rte(cx.pstmt, *scanrelid as u32) else {
        return Ok(refuse("hosts", "the bitmap heap scan's range table entry"));
    };
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let mut found = mcx::PgVec::new_in(cx.mcx);
    let shape = leaf_recipes(cx, &heap, r.rellockmode, plan, &mut found);
    let range = morsel.then(|| RangeRecipe::from_plan_byref(&heap, cx.attnos, &lane_words(cx.site, gravity_abi::ctx::lane::KIND_BYREF)));
    heap.close(NoLock)?;
    // D2988 (1): the heap scan's read-ahead bound (D583, as an index walk's): a bounded scan (a constant LIMIT above, a
    // single-match inner) reads one heap page a window, never pages its consumer does not pull (EXPLAIN's Heap Blocks)
    let budget = gravity_hosts::ops::scan::index_window::limit_budget(cx.pstmt, cx.node_id);
    let mut leaves: Vec<SiteRecipe> = found.into_iter().map(|l| l.with_row_budget(budget)).collect(); // alloc: Program
    let shape = match shape? {
        Ok(s) => s,
        Err(why) => return Ok(Err(why)),
    };
    if let Some(range) = range {
        let range = match range {
            Ok(r) => r,
            Err(e) => return Ok(refuse("hosts", e.message())),
        };
        let bitmap = match (plan, shape) {
            (BitmapPlan::Index { .. }, Shape::Leaf(_)) => MorselBitmap::Leaf(leaves.remove(0)),
            (_, shape) => MorselBitmap::Tree(TreeRecipe { leaves, shape }),
        };
        return Ok(Ok(Some(RecipeTpl::Rung(Arc::new(Local(MorselTpl { range, bitmap })))))); // alloc: Program
    }
    let heap = plan_recheck(cx);
    Ok(Ok(Some(match (plan, shape) {
        (BitmapPlan::Index { indexid, node_id, .. }, Shape::Leaf(_)) => {
            let (heap, words) = match heap {
                Some(h) => (Some(h), None),
                None => plan_param(cx, leaves[0].words.len()).map_or((None, None), |(h, w)| (Some(h), Some(Arc::new(w)))), // alloc: Program
            };
            RecipeTpl::Bitmap(leaves.remove(0), cx.site.relation, *indexid, *node_id, heap, words)
        }
        (BitmapPlan::And { node_id, .. } | BitmapPlan::Or { node_id, .. }, shape) => {
            RecipeTpl::Rung(Arc::new(Local(TreeTpl { tree: TreeRecipe { leaves, shape }, relid: cx.site.relation, root: *node_id, heap }))) // alloc: Program
        }
        _ => return Ok(refuse("hosts", "a bitmap plan whose shape is not its tree's")),
    })))
}

/// The tree's leaves (`BitmapIndexScan.indexqual` the keys, any index AM) in plan order, and its shape over them.
fn leaf_recipes(cx: &AdmitCx<'_>, heap: &types_rel::Relation<'_>, lock: i32, plan: &BitmapPlan<'_>, out: &mut mcx::PgVec<'_, SiteRecipe>) -> types_error::PgResult<Result<Shape, Refused>> {
    stack_depth_core::check_stack_depth()?;
    match plan {
        BitmapPlan::Index { node_id, indexid, .. } => {
            // an order-free index scan as the morsel form (decompose closure.rs `index_morsel`) is its own leaf
            let Some(q) = plan_node(cx.pstmt, *node_id).and_then(|n| n.as_bitmap_index_scan().map(|s| &s.indexqual).or_else(|| n.as_index_scan().map(|s| &s.indexqual))) else {
                return Ok(refuse("hosts", "the bitmap index scan's clauses"));
            };
            let index = indexam::index_open(cx.mcx, *indexid as Oid, lock)?;
            let recipe = SiteRecipe::from_plan_any_am(heap, &index, q, &NodeList::nil(), lock, cx.attnos);
            index.close(NoLock)?;
            match recipe {
                Ok(r) => mcx::GrowIn::push_in(out, r),
                Err(e) => return Ok(refuse("hosts", e.message())),
            }
            Ok(Ok(Shape::Leaf(out.len() - 1)))
        }
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => {
            let mut kids = Vec::with_capacity(plans.len()); // alloc: Program
            for p in plans.iter() {
                match leaf_recipes(cx, heap, lock, p, out)? {
                    Ok(s) => kids.push(s), // alloc: Program
                    Err(why) => return Ok(Err(why)),
                }
            }
            Ok(Ok(if matches!(plan, BitmapPlan::And { .. }) { Shape::And(kids) } else { Shape::Or(kids) }))
        }
    }
}

/// A btree walk (or index-only walk) with an `= ANY` key over a parameter's or an expression's array (src.index_array /
/// src.index_only; V4R-44): M1's arm leaves it (V4R-36). The recipe is M1's btree recipe, the array bound at every seek
/// from its key word (hosts `SiteRecipe::from_plan`); M1's BIND files it (`RecipeTpl::Index`).
fn index_array(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Index { scanrelid, indexid, backward, only, keys, iam: IndexAm::Btree, .. } = cx.source else { return Ok(Ok(None)) };
    if !bound_array(keys) || gravity_lowerv4::rerun::params_key::hosts_bound(keys, &cx.site.param_slots) {
        return Ok(Ok(None));
    }
    let Some(r) = shapes::rte(cx.pstmt, *scanrelid as u32) else {
        return Ok(refuse("hosts", "the index scan's range table entry"));
    };
    let Some(indexqual) = plan_node(cx.pstmt, cx.node_id).and_then(|n| n.as_index_scan().map(|s| &s.indexqual).or_else(|| n.as_index_only_scan().map(|s| &s.indexqual))) else {
        return Ok(refuse("hosts", "the index scan's clauses"));
    };
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let index = indexam::index_open(cx.mcx, *indexid as Oid, r.rellockmode)?;
    // V4R-108: a constant array is a key word too (hosts `from_plan_words`), never the recipe's image
    let recipe = SiteRecipe::from_plan_words(&heap, &index, indexqual, r.rellockmode, cx.attnos).map(|r| r.with_backward(*backward).with_only(*only).with_row_budget(gravity_hosts::ops::scan::index_window::limit_budget(cx.pstmt, cx.node_id)).with_run_bound(gravity_hosts::ops::scan::index_window::run_bound(cx.pstmt, cx.node_id)));
    index.close(NoLock)?;
    heap.close(NoLock)?;
    Ok(recipe.map(|r| Some(RecipeTpl::Index(r))).or_else(|e| refuse("hosts", e.message())))
}

/// The btree keys this arm owns: some `= ANY` key over a non-constant array, no AM or ORDER BY operator key, and no key or array computed from executor parameters (D1016 rows 1 and 2: M8's host cell, which this recipe never binds).
fn bound_array(keys: &[IndexKey<'_>]) -> bool {
    keys.iter().any(|k| k.form == KeyForm::Any && k.value.as_const().is_none()) && !keys.iter().any(|k| matches!(k.form, KeyForm::AmOp | KeyForm::AmAny | KeyForm::OrderBy) || (matches!(k.form, KeyForm::Op | KeyForm::Any) && gravity_lowerv4::rerun::params_key::computed(k.value)) || (k.form == KeyForm::Row && k.value.as_row_compare_expr().is_some_and(|rc| rc.rargs.iter().any(gravity_lowerv4::rerun::params_key::computed))))
}

fn tid(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Tid { form, quals, scanrelid, am: Am::Heap, .. } = cx.source else { return Ok(Ok(None)) };
    if crate::ops::scan::param_tid::ours(*form, quals, *scanrelid as i32, &crate::ops::scan::param_tid::host_slot(cx)) { return Ok(Ok(None)) } // D1016 / D1630: a value the host's is param_tid.rs's
    let mut clauses = Vec::with_capacity(quals.len()); // alloc: Program
    for q in quals.iter() {
        match tid_clause(q, *scanrelid as i32, *form) {
            Some(c) => clauses.push(c), // alloc: Program
            None => return Ok(refuse("hosts", "a tid clause outside the tid comparison set")),
        }
    }
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let recipe = TidRecipe::from_plan(&heap, cx.attnos, clauses, *form == TidForm::Range);
    heap.close(NoLock)?;
    Ok(recipe.map(|r| Some(RecipeTpl::Rung(Arc::new(r)))).or_else(|e| refuse("hosts", e.message()))) // alloc: Program
}

/// One tid clause as nodeTidscan.c / nodeTidrangescan.c read it: `ctid = v`, `ctid = ANY(a)`, a bound (the operator
/// commuted when ctid is on the right: `v < ctid` is a lower bound), `CURRENT OF`.
pub(crate) fn tid_clause(q: Node<'_>, varno: i32, form: TidForm) -> Option<TidClause> {
    if let Some(c) = q.as_current_of_expr() {
        // D422: the cursor is the clause's key word (bound per execution), never the recipe's
        return Some(TidClause::Current { by_param: c.cursor_name.is_none(), param: c.cursor_param });
    }
    if q.as_scalar_array_op_expr().is_some_and(|s| s.useOr && tid_op(s.opno) == Some(TidOp::Eq)) {
        return (form == TidForm::Points).then_some(TidClause::Any);
    }
    let op = q.as_op_expr()?;
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varattno == -1);
    let flip = !is_ctid(op.args.nth(0));
    match (form, tid_op(op.opno)) {
        (TidForm::Points, Some(TidOp::Eq)) => Some(TidClause::Eq),
        (TidForm::Range, Some(TidOp::Less { inclusive })) if !flip => Some(TidClause::Upper { inclusive }),
        (TidForm::Range, Some(TidOp::Less { inclusive })) => Some(TidClause::Lower { inclusive }),
        (TidForm::Range, Some(TidOp::Greater { inclusive })) if !flip => Some(TidClause::Lower { inclusive }),
        (TidForm::Range, Some(TidOp::Greater { inclusive })) => Some(TidClause::Upper { inclusive }),
        _ => None,
    }
}

fn sample(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Sample { sample, am: Am::Heap, .. } = cx.source else { return Ok(Ok(None)) };
    if crate::ops::scan::param_sample::ours(sample) { return Ok(Ok(None)) } // D1016 row 3: a computed argument is M8's (m8_param_sample.rs)
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    if cx.site.rows_via_am {
        // D4735: the AM's sample scan, PostgreSQL's sampler driving it (hosts am_rows.rs)
        let recipe = AmRowsRecipe::from_plan(&heap, cx.attnos, &lane_words(cx.site, gravity_abi::ctx::lane::KIND_BYREF))
            .and_then(|r| r.at_node(cx.node_id).with_sample(cx.mcx, sample.handler as Oid, sample.repeatable.is_some(), sample.args as *const _ as usize));
        heap.close(NoLock)?;
        return rows_tpl(recipe);
    }
    let range = RangeRecipe::from_plan_byref(&heap, cx.attnos, &lane_words(cx.site, gravity_abi::ctx::lane::KIND_BYREF));
    heap.close(NoLock)?;
    // D1920 / D1644: the plan node's identity (its argument list's address: every site mapping that node shares it), whose one
    // seed the bind takes from this execution's registry
    let recipe = range.and_then(|range| SampleRecipe::from_plan(range, cx.mcx, sample.handler as Oid, sample.repeatable.is_some(), sample.args as *const _ as usize));
    Ok(recipe.map(|r| Some(RecipeTpl::Rung(Arc::new(Local(r))))).or_else(|e| refuse("hosts", e.message()))) // alloc: Program
}

/// Recipe `t` into this execution's registry; a bitmap's recheck qual compiled here (its params are this execution's).
fn bind(t: &RecipeTpl, reg: &mut HostRegistry, cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs) -> BindResult {
    if let Some(r) = crate::ops::scan::bitmap_param::bind(t, reg, cx, exprs)? { return Ok(r) } // D1277: a recheck over a key's parameter
    match t {
        RecipeTpl::Range(r) => reg.push_range(r.clone()),
        // D2207: the site's own scan by its node id (two scans over one index in a statement have their own quals)
        RecipeTpl::Bitmap(r, relid, _, node, heap, _) => match recheck(cx, exprs, *relid, (*heap, *node), false, |p| matches!(p, BitmapPlan::Index { node_id, .. } if node_id == node))? {
            // D2988: the leaf's pre-check of its BitmapHeapScan's qual (sites.rs `bitmap_prefilter`)
            Ok((f, q)) => reg.push_bitmap(crate::ops::scan::sites::bitmap_prefilter(r.bind_copy(), q, cx, exprs)?, f),
            Err(why) => return Ok(Err(why)),
        },
        RecipeTpl::Rung(any) => {
            if let Some(Local(t)) = any.downcast_ref::<Local<TreeTpl>>() {
                if let Some(r) = crate::ops::scan::bitmap_param::bind_tree(&t.tree, t.relid, t.root, reg, cx, exprs)? { return Ok(r) } // W4a (V4R-550): a recheck over the leaves' parameters
                let root = |p: &BitmapPlan<'_>| matches!(p, BitmapPlan::And { node_id, .. } | BitmapPlan::Or { node_id, .. } if *node_id == t.root);
                match recheck(cx, exprs, t.relid, (t.heap, t.root), true, root)? {
                    Ok((f, _)) => reg.push_rung(Box::new(BitmapTreeRecipe { tree: t.tree.clone(), recheck: f })), // alloc: Program
                    Err(why) => return Ok(Err(why)),
                }
            } else if let Some(Local(t)) = any.downcast_ref::<Local<MorselTpl>>() {
                // A2 entry 2: an instrumented execution keeps the leader's leaf walk (EXPLAIN ANALYZE's Heap Blocks are PG's)
                reg.push_rung(Box::new(BitmapMorselRecipe::new(t.range.clone(), t.bitmap.clone()).walking_wide(!crate::admit::is_instrumented()))); // alloc: Program
            } else if let Some(r) = any.downcast_ref::<TidRecipe>() {
                reg.push_rung(Box::new(r.clone())); // alloc: Program
            } else if let Some(r) = any.downcast_ref::<AmRowsRecipe>() {
                let r = r.with_quals(rows_quals(cx, r)).bound(reg); // D4735: a sample's with its plan node's one seed
                reg.push_rung(Box::new(r)); // alloc: Program
            } else if let Some(Local(r)) = any.downcast_ref::<Local<SampleRecipe>>() {
                let r = r.bound(reg); // D1920 / D1644: the plan node's one seed in this execution
                reg.push_rung(Box::new(r)); // alloc: Program
            } else {
                return Ok(Ok(false));
            }
        }
        _ => return Ok(Ok(false)),
    }
    Ok(Ok(true))
}

/// Read from this execution's plan (`BindCx.plan`), never the admission's: a literal is the execution's, and a front-key
/// hit shares the template.
fn rows_quals(cx: &BindCx<'_>, r: &AmRowsRecipe) -> Vec<tableam_vocab::ZoneQual> {
    let Some(pstmt) = cx.plan else { return Vec::new() }; // alloc: Exec
    let Some(ss) = plan_node(pstmt, r.node_id).and_then(|n| n.as_seq_scan()) else { return Vec::new() }; // alloc: Exec
    if !shapes::rte(pstmt, ss.scan.scanrelid as u32).is_some_and(|e| e.relid == r.heap) {
        return Vec::new(); // alloc: Exec
    }
    pushed_quals(ss).into_iter().map(|(_, q)| q).collect() // alloc: Exec
}

/// One is pushed when it is lowerv4's rows-road form (a column, the native's zone comparison, an IMMUTABLE and LEAKPROOF
/// operator, no collation) over a non-NULL by-value constant. The AM compares datum words and casts nothing; the program
/// keeps evaluating the whole qual: the AM only skips rows that cannot pass.
pub(crate) fn pushed_quals(ss: &types_nodes::plannodes::SeqScan<'_>) -> Vec<(u32, tableam_vocab::ZoneQual)> {
    use gravity_decompose::ops::shared::scan::zone_word;
    let mut out = Vec::new(); // alloc: Exec
    for (i, q) in ss.scan.plan.qual.iter().enumerate() {
        let Some((attno, op, c)) = gravity_lowerv4::ops::scan::heap::rows_zone_form(q, ss.scan.scanrelid as i32) else { continue };
        let (Some(val), Some(op), Ok(attnum), Ok(i)) = (c.as_const().and_then(zone_word), zone_cmp(op), u16::try_from(attno), u32::try_from(i)) else { continue };
        out.push((i, tableam_vocab::ZoneQual { attnum, op, val })); // alloc: Exec
    }
    out
}

fn zone_cmp(op: gravity_program::ZoneOp) -> Option<tableam_vocab::ZoneCmp> {
    use gravity_program::ZoneOp;
    use tableam_vocab::ZoneCmp;
    Some(match op {
        ZoneOp::Eq => ZoneCmp::Eq,
        ZoneOp::Ne => ZoneCmp::Ne,
        ZoneOp::Lt => ZoneCmp::Lt,
        ZoneOp::Le => ZoneCmp::Le,
        ZoneOp::Gt => ZoneCmp::Gt,
        ZoneOp::Ge => ZoneCmp::Ge,
        _ => return None,
    })
}

/// Never an InitPlan's parameter, which stays lazy.
fn pushed_at(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, ss: &types_nodes::plannodes::SeqScan<'_>) -> Vec<u32> {
    use types_nodes::primnodes::ParamKind;
    let mut out: Vec<u32> = pushed_quals(ss).into_iter().map(|(i, _)| i).collect(); // alloc: Cold
    let word = |t: u32| gravity_decompose::catalog::typefacts::row_of(t).is_some_and(|r| r.byval && [1, 2, 4, 8].contains(&r.typlen));
    for (i, q) in ss.scan.plan.qual.iter().enumerate() {
        let Some(p) = gravity_lowerv4::ops::scan::heap::rows_zone_form(q, ss.scan.scanrelid as i32).filter(|f| zone_cmp(f.1).is_some()).and_then(|f| f.2.as_param()) else { continue };
        let read = match p.paramkind {
            ParamKind::PARAM_EXTERN => true,
            ParamKind::PARAM_EXEC => !init_param(pstmt, p.paramid),
            _ => false,
        };
        if read && word(p.paramtype) {
            out.push(i as u32); // alloc: Cold
        }
    }
    out.sort_unstable();
    out
}

fn init_param(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, id: i32) -> bool {
    let sets = |s: Node<'static>| s.as_sub_plan().is_some_and(|s| s.setParam.iter().any(|x| x == id));
    plan_find(pstmt, &|n| n.as_plan().is_some_and(|p| p.initPlan.iter().any(sets))).is_some()
}

pub(crate) fn rows_pushed(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, rels: &[u32]) -> Vec<(i32, u32)> {
    let out = core::cell::RefCell::new(Vec::new()); // alloc: Cold
    plan_find(pstmt, &|n| {
        if let Some(ss) = n.as_seq_scan() {
            if shapes::rte(pstmt, ss.scan.scanrelid as u32).is_some_and(|e| rels.contains(&e.relid)) {
                out.borrow_mut().extend(pushed_at(pstmt, ss).into_iter().map(|i| (ss.scan.plan.plan_node_id, i))); // alloc: Cold
            }
        }
        false
    });
    out.into_inner()
}

/// The `bitmapqualorig` of this execution's bitmap source over `relid` whose plan `is` picks, compiled; refused by name
/// when it reads an executor parameter or calls a volatile function. D2754: the main registry on a front-key hit (no
/// graph, `BindCx.front`) reads it from its own plan's BitmapHeapScan `heap.0` over `relid` whose bitmap subtree is plan
/// node `heap.1` (the admission's `plan_recheck`); an InitPlan's or an Append arm's registry is refused as before.
fn recheck(cx: &BindCx<'_>, exprs: &mut shapes::KeyExprs, relid: u32, heap: (Option<i32>, i32), held: bool, is: impl Fn(&BitmapPlan<'_>) -> bool) -> types_error::PgResult<Result<(Option<gravity_hosts::FilterFn>, &'static NodeList<'static>), Refused>> {
    let q = match (cx.graph, cx.front.zip(heap.0)) {
        (Some(graph), _) => {
            let mut all = cx.charged_vec();
            all.with_vec(|v| graph.pipelines.iter().for_each(|p| sources(p, v))); // alloc: Exec charged cx.owner
            let Some(q) = all.iter().find_map(|(s, _)| match s {
                Source::Bitmap { relid: r, plan, recheck, .. } if *r == relid && is(plan) => Some(*recheck),
                _ => None,
            }) else {
                return Ok(refuse("hosts", "a bitmap recipe without its source in this execution's graph"));
            };
            q
        }
        (None, Some((pstmt, id))) => {
            let Some(q) = front_qual(pstmt, id, relid, heap.1) else {
                return Ok(refuse("hosts", "a bitmap recipe without its source in this execution's plan"));
            };
            q
        }
        (None, None) => return Ok(refuse("hosts", "a bitmap scan under an InitPlan or an Append arm (the recheck rebinds from the main graph)")),
    };
    if q.iter().any(|n| clauses::contain_exec_params(n).unwrap_or(true)) {
        return Ok(refuse("hosts", "a bitmap recheck over an executor parameter (an InitPlan or nest-loop value)"));
    }
    // `held`: the recipe keeps the qual for its Node's life (a tree's rung), a closure over this admission's slots that
    // refuses the park (`filters`); else a one-index bitmap's source takes each execution's at its bind (hosts
    // bitmap_source.rs) and a parked Node holds none (`rechecks`)
    let f = shapes::compile_filter_bound(cx.mcx, q, shapes::param_bind(exprs))?;
    let n = usize::from(matches!(f, Some(Some(_))));
    if held {
        exprs.filters += n;
    } else {
        exprs.rechecks += n;
    }
    Ok(f.map_or_else(|| refuse("hosts", "a bitmap recheck qual the host cannot compile (a volatile function)"), |f| Ok((f, q))))
}

/// D2754: a front-key hit's recheck: `bitmapqualorig` of its own plan's BitmapHeapScan `id`, which must scan `relid` above
/// the bitmap subtree at plan node `sub` (the same plan shape under the same front key); `None` when it does not.
pub(crate) fn front_qual(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, id: i32, relid: u32, sub: i32) -> Option<&'static NodeList<'static>> {
    let s = plan_node(pstmt, id)?.as_bitmap_heap_scan()?;
    let shape = shapes::rte(pstmt, s.scan.scanrelid as u32).is_some_and(|r| r.relid == relid) && s.scan.plan.lefttree.and_then(|t| t.as_plan()).is_some_and(|t| t.plan_node_id == sub);
    shape.then_some(&s.bitmapqualorig)
}

/// D2754 part 4: for a one-index bitmap whose recheck reads executor parameters, the BitmapHeapScan whose own
/// `bitmapqualorig` is that recheck (by identity, as `plan_recheck`) and the words bitmap_param.rs `bind` would bind it over
/// (`front_words`: no host-computed key, no hazard); a front-key hit compiles its own plan's recheck over them. `None`: the
/// recipe binds from the graph alone, as before.
fn plan_param(cx: &AdmitCx<'_>, n_words: usize) -> Option<(i32, crate::ops::scan::bitmap_param::ParamWords)> {
    let Source::Bitmap { recheck, .. } = cx.source else { return None };
    let words = crate::ops::scan::bitmap_param::front_words(cx.graph, cx.source, n_words)?;
    let n = plan_find(cx.pstmt, &|n| n.as_bitmap_heap_scan().is_some_and(|s| core::ptr::eq(&s.bitmapqualorig, *recheck)))?;
    Some((n.as_plan()?.plan_node_id, words))
}

/// D2754: the plan node id of the BitmapHeapScan whose own `bitmapqualorig` is this bitmap source's recheck (found by
/// identity, so decompose did not rebuild it), when BIND takes the plain recheck (bitmap_param.rs `plain`) and neither
/// that qual nor any leaf's index clauses read an executor parameter; a front-key hit, bound without the graph, reads the
/// recheck from that node of its own plan, the same shape under the same front key (`recheck`). `None`: the recipe binds
/// from the graph alone, as before.
fn plan_recheck(cx: &AdmitCx<'_>) -> Option<i32> {
    let Source::Bitmap { recheck, .. } = cx.source else { return None };
    if !crate::ops::scan::bitmap_param::plain(cx.graph, cx.source) {
        return None;
    }
    let n = plan_find(cx.pstmt, &|n| n.as_bitmap_heap_scan().is_some_and(|s| core::ptr::eq(&s.bitmapqualorig, *recheck)))?;
    let s = n.as_bitmap_heap_scan()?;
    let free = |l: &NodeList<'static>| l.iter().all(|x| !clauses::contain_exec_params(x).unwrap_or(true));
    (free(&s.bitmapqualorig) && leaves_free(s.scan.plan.lefttree?, &free)).then_some(s.scan.plan.plan_node_id)
}

/// D2988: the qual of the statement's BitmapHeapScan whose own `bitmapqualorig` is `recheck` (by identity, as
/// `plan_recheck`); a bitmap leaf's pre-check compiles it (sites.rs `bitmap_prefilter`).
pub(crate) fn heap_qual(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, recheck: &NodeList<'_>) -> Option<&'static NodeList<'static>> {
    let n = plan_find(pstmt, &|n| n.as_bitmap_heap_scan().is_some_and(|s| core::ptr::addr_eq(&s.bitmapqualorig, recheck)))?;
    n.as_bitmap_heap_scan().map(|s| &s.scan.plan.qual)
}

/// Whether every BitmapIndexScan under bitmap subtree `n` has clauses `free` accepts (anything but a BitmapIndexScan,
/// BitmapAnd or BitmapOr answers no).
fn leaves_free(n: Node<'static>, free: &dyn Fn(&NodeList<'static>) -> bool) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    if let Some(s) = n.as_bitmap_index_scan() {
        return free(&s.indexqual) && free(&s.indexqualorig);
    }
    let kids = n.as_bitmap_and().map(|a| &a.bitmapplans).or_else(|| n.as_bitmap_or().map(|o| &o.bitmapplans));
    kids.is_some_and(|l| l.iter().all(|c| leaves_free(c, free)))
}

/// The plan node numbered `id` in the statement (the main tree or a subplan).
pub(crate) fn plan_node(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, id: i32) -> Option<Node<'static>> {
    plan_find(pstmt, &|n| n.as_plan().is_some_and(|p| p.plan_node_id == id))
}

/// The first plan node `is` accepts in the statement (the main tree, then each subplan), in plan order.
fn plan_find(pstmt: &'static types_nodes::plannodes::PlannedStmt<'static>, is: &dyn Fn(Node<'static>) -> bool) -> Option<Node<'static>> {
    fn under(n: Option<Node<'static>>, is: &dyn Fn(Node<'static>) -> bool) -> Option<Node<'static>> {
        stack_depth_core::check_stack_depth_or_panic();
        let n = n?;
        let p = n.as_plan()?;
        if is(n) {
            return Some(n);
        }
        let kids = [n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|o| &o.bitmapplans), n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, is).or_else(|| under(p.righttree, is)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), is)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, is)))
    }
    under(pstmt.planTree, is).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, is)))
}

/// The site's attribute lanes of lane kind `kind` as a bit set.
fn lane_words(site: &SiteDesc, kind: u8) -> [u64; 64] {
    let mut w = [0u64; 64];
    for (k, l) in site.lanes().iter().enumerate().take(site.natts().min(64 * 64)) {
        if l.kind == kind {
            w[k / 64] |= 1 << (k % 64);
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V4R-44: a btree walk with an `= ANY` key over a non-constant array is this arm's; constant arrays, plain keys and
    /// AM / ORDER BY operator keys are not (M1's arm, or the any-AM arm).
    #[test]
    fn a_bound_array_key_is_this_arms() {
        let ctx = mcx::MemoryContext::new("v4r-40");
        let mcx = ctx.mcx();
        let c = types_nodes::Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(1), false, true).unwrap();
        let v = types_nodes::Node::mk_var(mcx, 1, 1, types_core::INT4OID, -1, 0, 0).unwrap();
        let key = |form, value| IndexKey { attno: 1, col: 1, opno: 96, value, form };
        assert!(bound_array(&[key(KeyForm::Op, v), key(KeyForm::Any, v)]), "a parameter array");
        assert!(!bound_array(&[key(KeyForm::Any, c)]), "a constant array stays M1's");
        assert!(!bound_array(&[key(KeyForm::Op, v)]), "no array");
        assert!(!bound_array(&[key(KeyForm::Any, v), key(KeyForm::OrderBy, c)]), "a KNN walk is the any-AM arm's");
        // D1016: a key computed from executor parameters beside the array is M8's host cell, never this recipe's word
        let p = types_nodes::Node::mk(mcx, types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXEC, paramid: 0, paramtype: types_core::INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let computed = types_nodes::Node::mk_relabel_type(mcx, p, types_core::INT4OID, -1, 0, types_nodes::primnodes::CoercionForm::COERCE_IMPLICIT_CAST).unwrap();
        assert!(!bound_array(&[key(KeyForm::Any, v), key(KeyForm::Op, computed)]), "a computed key beside a parameter array");
        assert!(bound_array(&[key(KeyForm::Any, v), key(KeyForm::Op, p)]), "a bare parameter key beside it stays this arm's");
        // D1016 row 2: an array computed from executor parameters is M8's host cell too, beside a bare parameter's array
        assert!(!bound_array(&[key(KeyForm::Any, p), key(KeyForm::Any, computed)]), "a computed array beside a parameter array");
        assert!(!bound_array(&[key(KeyForm::Any, computed)]), "a computed array alone");
        assert!(bound_array(&[key(KeyForm::Any, p)]), "a bare parameter array stays this arm's");
    }

    #[test]
    fn the_rows_road_pushes_bool_oid_time_and_params() {
        use gravity_hosts::ops::scan::am_rows::rows_param_of;
        use gravity_lowerv4::ops::scan::heap::{rows_param_desc, rows_zone_form};
        use gravity_program::ZoneOp;
        use types_core::{BOOLOID, CHAROID, OIDOID, TEXTOID, TIMEOID};
        let ctx = mcx::MemoryContext::new("pszp");
        let mcx = ctx.mcx();
        let op = |opno: u32, coll: u32, l, r| {
            let args = NodeList::from_slice(mcx, &[l, r]).unwrap();
            types_nodes::Node::mk(mcx, types_nodes::primnodes::OpExpr { opno, opfuncid: 0, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: coll, args, location: -1 }).unwrap()
        };
        let var = |a: i16, t: u32| types_nodes::Node::mk_var(mcx, 1, a, t, -1, 0, 0).unwrap();
        let k = |t: u32, len: i32, d: datum::Datum| types_nodes::Node::mk_const(mcx, t, -1, 0, len, d, false, true).unwrap();
        let form = |q| rows_zone_form(q, 1).map(|(a, o, c)| (a, o, c.as_const().and_then(gravity_decompose::ops::shared::scan::zone_word)));
        // boollt 58, oideq 607, oidlt 609 (o > 4000000000: zero-extended), time_lt 1110, chareq 92, texteq 98
        assert_eq!(form(op(58, 0, var(1, BOOLOID), k(BOOLOID, 1, datum::Datum::from_bool(true)))), Some((1, ZoneOp::Lt, Some(1))));
        assert_eq!(form(op(607, 0, var(2, OIDOID), k(OIDOID, 4, datum::Datum::from_u32(1234)))), Some((2, ZoneOp::Eq, Some(1234))));
        assert_eq!(form(op(609, 0, k(OIDOID, 4, datum::Datum::from_u32(4_000_000_000)), var(2, OIDOID))), Some((2, ZoneOp::Gt, Some(4_000_000_000))));
        assert_eq!(form(op(1110, 0, var(3, TIMEOID), k(TIMEOID, 8, datum::Datum::from_i64(60_000_000)))), Some((3, ZoneOp::Lt, Some(60_000_000))));
        assert_eq!(form(op(92, 0, var(4, CHAROID), k(CHAROID, 1, datum::Datum::from_i8(66)))), None, "\"char\" is never pushed");
        assert!(rows_zone_form(op(98, 100, var(5, TEXTOID), var(6, TEXTOID)), 1).is_none(), "a collated comparison is never pushed");
        for (attno, z, len, typid) in [(1u16, ZoneOp::Lt, 1u8, BOOLOID), (7, ZoneOp::Ne, 8, types_core::INT8OID), (2, ZoneOp::Ge, 4, OIDOID)] {
            let (a, cmp, l, t) = rows_param_of(rows_param_desc(attno, z, len, typid) as u64).unwrap();
            assert_eq!((a, format!("{cmp:?}"), l, t), (attno, format!("{z:?}"), len as i32, typid)); // alloc: Cold
        }
        assert!(rows_param_of(rows_param_desc(1, ZoneOp::NotNull, 8, types_core::INT8OID) as u64).is_none(), "no word for IS NOT NULL");
    }
}
