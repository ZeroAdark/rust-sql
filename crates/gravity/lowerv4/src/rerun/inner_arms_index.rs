//! M8-NEST S2c (V4R-542, D2015's dominant form; a helper of nest/inner_arms.rs, split for its budget): an arm of a union a
//! nest (or nested SubPlan) inner embeds that is a btree Index or Index Only Scan. S2b's `Arms` makes each arm's source by
//! its own kind's maker (source/index.rs, source/index_only.rs) under the inner's identity and opens it at the arm's first
//! read of every re-open, so the walk's keys are stored again then (frame/site.rs `store_keys`): a bare executor parameter
//! from the words the owner wrote for this outer row, an InitPlan's through its handle, a constant or an extern parameter
//! from its bind word. That is trunk's lone index inner path, one arm at a time. Admitted here, in order (the first failure
//! names the refusal): a btree walk (backward too: PSP3-11) over a table with an index fetch, keys `column op value`, `IS [NOT] NULL` or `column = ANY (array)` (a constant or extern
//! parameter array, D3877) only (D2015);
//! each key's value a constant, a bare parameter or an expression over executor parameters, as the makers place it
//! (source/index.rs `key_late`; D2013 slice 2: the computed one the host's at the arm's open, params_key.rs `per_open`, as
//! ExecReScan's ExecIndexEvalRuntimeKeys at the arm's first ExecProcNode after the Append's rescan), any other expression
//! by D1630's name (bound once at bind: an arm's walk drives no program pipeline); no row lock and no write but a plain
//! INSERT anywhere in the statement (EvalPlanQual would recheck an arm's index quals; D2015; D3680: ExecInsert runs
//! none). `site` numbers the arm's site only where exec admit's pairing (keyed by (relation, index), a bitmap by its
//! probe index), replayed over the sites so far with each
//! number an owner holds counted as a source of the owner's key (spool/site.rs `pairs_held`), gives it its own feed's
//! source (or, a feed exec admit walks before its owner, at the owner's held number with the owner moved on:
//! `shift_held`): an owner or another scan on that index paired ahead of it would hand the arm its recipe (D1633's class), two
//! arms on one index pair in arm order, and a remade inner's second copy of such an arm refuses (D2015). A user operator, opclass or
//! expression-index function is user code reached indirectly (the deferred volatile family): served. Registers
//! nothing. Owner M8-NEST. A one-index btree bitmap heap scan arm (landing 10, D2015's form over partition leaves) is
//! admitted by the same rules over its leaf's keys and paired by its probe index (spool/site.rs `key`), beside another
//! one-index bitmap scan over its index too (D2207 fixed: each rechecks with its own qual).
//! Also here (inner_arms.rs's budget): `lone_range`, S2b's heap-arm pairing rule keyed as exec admit keys, so a heap arm
//! beside index arms or an owner's index scan of its relation serves (D2015 before). What this file newly serves (an
//! index or bitmap arm, such a heap arm) is served beside a hash or merge join too: D2194's no-widen name left at item
//! 14 part 4 (D2325 fixed, CAND-M39 not reproduced), as D2178 / D2179, D2180, D2197 and D2213 left at their root fixes.
use gravity_decompose::graph::KeyForm;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{IndexAm, Op, PipelineNode, Source};
use gravity_program::{Cmd, Rep, SiteDesc};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::ops::scan::index::index_fetch;
use crate::rerun::inner_arms::UNION_ARM_BEYOND_FORMS;
use crate::rerun::spool::site::{key, pairs_held, pairs_held_at, shift_held, walk, Entry};
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// inner_arms.rs `admit_arm`'s source check for a source other than a heap range, a Result or VALUES: an index arm
/// admitted here (module doc), else the refusal's name.
pub(crate) fn admit(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, node: i32) {
    let mut keys = PgVec::new_in(gx.mcx);
    match &p.source {
        Source::Index { am, relid, iam: IndexAm::Btree, keys: ks, .. } if index_fetch(gx, *am, *relid) => keys.push_in(&ks[..]),
        // errorhold (rnd-0194): a BitmapAnd / BitmapOr tree of btree leaves too (bitmap_tree.rs: each leaf's keys in leaf
        // order, the site keyed by the decomposer's index, as exec pairs it)
        Source::Bitmap { am, relid, plan, .. } if index_fetch(gx, *am, *relid) && btree_leaves(plan, &mut keys) => {}
        _ => shape(node, UNION_ARM_BEYOND_FORMS),
    }
    let keys: PgVec<'_, _> = keys.into_iter().flatten().collect_in(gx.mcx);
    // D3877: `column = ANY (array)` too, the array a constant or an extern parameter: one word bound at the seek, the same
    // at every re-open (index_only.rs `key_words`)
    let bound = |v: Node<'_>| v.as_const().is_some() || v.as_param().is_some_and(|p| matches!(p.paramkind, ParamKind::PARAM_EXTERN));
    if !keys.iter().all(|k| matches!(k.form, KeyForm::Op | KeyForm::Null | KeyForm::NotNull) || (k.form == KeyForm::Any && bound(k.value))) {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
    keys.iter().filter(|k| k.form == KeyForm::Op).for_each(|k| bare(k.value, node));
    // D2015: but a MERGE's result relations' arms, rechecked through their keys at the substituted
    // open (merge/nest_arms.rs `covers`); lockrows NEST-CLAIM: or the result relations' arms of an UPDATE / DELETE's union
    // form (merge/nest_claim.rs `covers`); D3680: a statement whose writes are plain INSERTs runs no recheck (`rechecks`)
    if gx.pipelines().iter().any(rechecks) && !crate::ops::modifytable::merge::nest_arms::covers(gx, p)
        && !crate::ops::modifytable::merge::nest_claim::covers(gx, p)
    {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
}

/// Bitmap tree `p`'s leaves are btree index scans: each leaf's keys pushed onto `out` in leaf order; false on another leaf.
fn btree_leaves<'a, 'p>(p: &'a BitmapPlan<'p>, out: &mut PgVec<'_, &'a [gravity_decompose::graph::IndexKey<'p>]>) -> bool {
    crate::stack_check();
    match p {
        BitmapPlan::Index { iam: IndexAm::Btree, keys, .. } => {
            out.push_in(&keys[..]);
            true
        }
        BitmapPlan::Index { .. } => false,
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => !plans.is_empty() && plans.iter().all(|q| btree_leaves(q, out)),
    }
}

/// A key's value as the makers place it (lower words.rs `key_word`): a constant or an extern or executor parameter is its
/// own word (read at every open), an expression over executor parameters the host's at every open (D2013 slice 2), any
/// other bound once (D1630).
fn bare(v: Node<'_>, node: i32) {
    if bare_value(v) {
        return;
    }
    shape(node, crate::rerun::spool::form::WALK_BOUND_AT_BIND)
}

/// A key value the makers place (`bare`'s test).
fn bare_value(v: Node<'_>) -> bool {
    v.as_const().is_some() || v.as_param().is_some_and(|p| matches!(p.paramkind, ParamKind::PARAM_EXTERN | ParamKind::PARAM_EXEC)) || crate::rerun::params_key::computed(v)
}

/// D3377 road (3) (union_arms.rs `arms_read`): `admit`'s test without its refusal, in a statement that writes and locks
/// nothing: a btree walk or a btree-leaved bitmap heap scan whose keys are `column op value` or `IS [NOT] NULL`, each value
/// one the makers place.
pub(crate) fn fits(gx: &Gx<'_, '_>, p: &PipelineNode<'_>) -> bool {
    let mut keys = PgVec::new_in(gx.mcx);
    match &p.source {
        Source::Index { am, relid, iam: IndexAm::Btree, keys: ks, .. } if index_fetch(gx, *am, *relid) => keys.push_in(&ks[..]),
        Source::Bitmap { am, relid, plan, .. } if index_fetch(gx, *am, *relid) && btree_leaves(plan, &mut keys) => {}
        _ => return false,
    }
    let key = |k: &gravity_decompose::graph::IndexKey<'_>| matches!(k.form, KeyForm::Null | KeyForm::NotNull) || (k.form == KeyForm::Op && bare_value(k.value));
    keys.into_iter().flatten().all(key) && !gx.pipelines().iter().any(writes)
}

/// D3680: `p` or an inner under it locks rows or writes other than a plain INSERT (one with no ON CONFLICT DO UPDATE): a
/// statement PostgreSQL may run EvalPlanQual for. ExecInsert runs none: a WITH CHECK OPTION's quals (an index arm of a
/// union its SubPlan reads, m8_d1112 :843) run once over the new row (ExecWithCheckOptions), as any other expression.
fn rechecks(p: &PipelineNode<'_>) -> bool {
    crate::stack_check(); // D3832
    p.ops.iter().any(|op| match op {
        Op::Lock(_) => true,
        Op::Write(w) => w.cmd != Cmd::Insert || w.on_conflict.as_ref().is_some_and(|c| c.update),
        Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => rechecks(inner),
        _ => false,
    })
}

/// `p` or an inner under it writes or locks rows (a ModifyTable, a LockRows; EvalPlanQual rechecks its scans' quals).
pub(crate) fn writes(p: &PipelineNode<'_>) -> bool {
    crate::stack_check(); // D3832
    p.ops.iter().any(|op| match op {
        Op::Write(_) | Op::Lock(_) => true,
        Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => writes(inner),
        _ => false,
    })
}

/// inner_arms.rs `admit_arm`'s pairing rule for a heap range arm (S2b's, D1633's class), keyed as exec admit keys: every
/// source its walk holds with the arm's (relation, index) is a heap range, whose recipe is its site's alone (they pair in
/// any order). An index or bitmap scan of that relation keys apart (by its index or probe index); a TID or sample scan
/// does not, and refuses the arm (D2015).
pub(crate) fn lone_range(gx: &Gx<'_, '_>, s: &Source<'_>) -> bool {
    let Source::Range { .. } = s else { return true };
    entries(gx).iter().all(|e| e.1 != key(s) || e.2)
}

/// Exec admit's walk of the graph's sources (nest/spool_site.rs `walk`).
pub(crate) fn entries<'g>(gx: &Gx<'g, '_>) -> PgVec<'g, Entry> {
    let ps = gx.pipelines();
    let (mut w, mut next) = (PgVec::new_in(gx.mcx), ps.len());
    ps.iter().enumerate().for_each(|(k, p)| walk(p, k, &mut next, &mut w));
    w
}

/// D3493: index (or bitmap) arm feed `kf`'s site number held at the feed's own make (inner_arms.rs `make_empty`, the
/// feed pipeline made in graph order) when exec admit walks the feed before another source of its (relation, index)
/// that is no inner union's feed and not on the inner's owner chain (`shift_held`'s case): a self-join's outer union
/// arm on the same partition and index (pg_overexplain's `vegetables v1, vegetables v2`: decompose settles the inner
/// union first, so its feeds come before the outer arms). That source's site is numbered at its make, before the inner
/// makes the arm, and exec admit would pair it with the feed's source (the arm's quals); held here, the arm's number
/// comes first, as exec admit walks them. `site` places the arm there. D3877: that source may be a nest inner of a later
/// pipeline too (the union's owner's own lower nest probing the same partition by the same index: its site is numbered
/// at that nest's make, before the owner's upper nest makes the arm).
pub(crate) fn hold(px: &mut Px<'_, '_>, kf: usize) {
    let gx = px.gx;
    let ps = gx.pipelines();
    let n = ps.len();
    let p = &ps[kf];
    let (Some(k), Some((e, _, _))) = (key(&p.source).filter(|_| matches!(p.source, Source::Index { .. } | Source::Bitmap { .. })), crate::pipes::inner_union(gx, kf)) else { return };
    let inners = ps.inners();
    let chain: PgVec<'_, usize> = core::iter::successors(inners.get(e.wrapping_sub(n)).map(|x| x.0 .0), |&o| (o >= n).then(|| inners.get(o - n).map(|x| x.0 .0)).flatten()).collect_in(gx.mcx);
    let w = entries(gx);
    // never once the arm is numbered (a remake of the feed): a held number nothing places would pair as its source
    let Some(own) = w.iter().position(|x| x.0 == kf).filter(|_| px.pipeline_site(kf).is_none()) else { return };
    let feed = |at: usize| at < n && crate::pipes::inner_union(gx, at).is_some_and(|u| u.2.contains(&at));
    if w[own + 1..].iter().any(|x| x.1 == Some(k) && !feed(x.0) && !chain.contains(&x.0)) {
        crate::pipes::reserve_site(px, kf);
    }
}

/// Arm feed `kf`'s site `d` numbered next (S2b's order); an index arm's only where exec admit pairs it with `kf`'s own
/// source after the sites numbered so far and the numbers owners hold, else at the first held number with the owners
/// moved on (spool/site.rs `shift_held`: a feed exec admit walks before an owner of its key), else D2015. D3493: a feed
/// that held its number (`hold`) takes it, where exec admit pairs it with its own source after the sites before it.
pub(crate) fn site(px: &mut Px<'_, '_>, kf: usize, d: SiteDesc) -> usize {
    let gx = px.gx;
    let p = &gx.pipelines()[kf];
    if !matches!(p.source, Source::Index { .. } | Source::Bitmap { .. }) {
        return px.add_site(d);
    }
    let w = entries(gx);
    let k = (d.relation, d.index);
    let Some(own) = w.iter().position(|e| e.0 == kf) else { shape(p.node_id, UNION_ARM_BEYOND_FORMS) };
    let held = px.ext::<crate::rerun::params::NestExt>(crate::face::kinds::Face::Nest).and_then(|x| x.sites.iter().find(|s| s.0 == kf)).map(|s| s.1);
    if let Some(at) = held {
        if pairs_held_at(px, &w, k, at) != Ok(Some(own)) {
            shape(p.node_id, UNION_ARM_BEYOND_FORMS);
        }
        return crate::pipes::place_site(px, kf, d);
    }
    // shapecheck fire signal: an arm whose key a source outside the inner unions' feeds shares (an owner's scan)
    let n = gx.pipelines().len();
    let feed = |at: usize| at < n && crate::pipes::inner_union(gx, at).is_some_and(|u| u.2.contains(&at));
    let shared = w.iter().any(|e| e.1 == Some(k) && !feed(e.0));
    if pairs_held(px, &w, k) == Ok(Some(own)) {
        if shared {
            fires::fire("nest.arm.shared_key");
        }
        return px.add_site(d);
    }
    let at = shift_held(px, &w, k, own, d).unwrap_or_else(|| shape(p.node_id, UNION_ARM_BEYOND_FORMS));
    fires::fire("nest.arm.shift_held");
    at
}

/// inner_arms.rs `make_empty`'s columns for an index feed: a heap fetch's are the relation's attributes (a heap range's),
/// an index-only walk's every index column, key or INCLUDE (source/index_only.rs `index_cols`), as its table attribute's;
/// an expression column (never read) and an attribute without facts are INT8's placeholder, as a heap range's.
pub(crate) fn cols<'g>(gx: &Gx<'g, '_>, s: &Source<'_>, rep: &dyn Fn(u32) -> Rep) -> PgVec<'g, (u32, Rep)> {
    let att = |relid: u32, a: i16| gx.facts.att(relid, a).filter(|_| a > 0).map_or((types_core::INT8OID, Rep::Word { bytes: 8 }), |f| (f.typid, rep(f.typid)));
    match s {
        Source::Index { only: true, relid, indexid, .. } => gx.facts.index(*indexid).map_or_else(|| PgVec::new_in(gx.mcx), |i| i.keys().iter().map(|&a| att(*relid, a)).collect_in(gx.mcx)),
        Source::Index { relid, .. } | Source::Bitmap { relid, .. } => (1..=gx.facts.rel(*relid).map_or(0, |r| r.natts.max(0))).map(|a| att(*relid, a)).collect_in(gx.mcx),
        _ => PgVec::new_in(gx.mcx),
    }
}
