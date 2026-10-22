//! write.update with its target hashed on a join's build side (returning-019 / returning-014, D838; D776's UPDATE half).
//! Owner M4-LOCK. A helper file (split from write/update.rs for its budget): it registers nothing.
//!
//! The emit scans a REFERENCE-marked relation and probes the target's hash (merge_admit.rs `built_target`: exactly one
//! producer scans the target, as MERGE's D776 form). The write's position is then the target's junk ctid column
//! (`LocFrom::JunkCtid`), not the driving scan's locator, and a recheck names that producer's scan site, rerun by the host
//! at the target's newest version (ExecUpdate's EvalPlanQual over the target's own row mark); the emit's own scan takes its
//! row from marks.rs's RowOf site.
//!
//! UNION: or an UPDATE of several result relations whose rows come out of one settled union of their
//! scans, read first (merge/route.rs `union_targets`, the form mergecmd G2(c) serves for MERGE: decompose settles the
//! Append under ModifyTable when it cannot stream the write into its arms). PostgreSQL's ModifyTable takes a row's
//! result relation from its junk tableoid (ExecLookupResultRelByOid) and its position from the junk ctid; a recheck
//! substitutes that relation's scan with the newest version and leaves every other result relation's scan empty
//! (EvalPlanQualStart's relsubs_blocked). So the position is the junk ctid at the first feed's site, the row's
//! relation its junk tableoid (`union_rel`), and the recheck code names that relation's feed site.
use gravity_abi::ctx::{site_off, POS_ROW, POS_VALID};
use gravity_abi::site::site;
use gravity_abi::status::{recheck, unsupported, UNSUPPORTED_SHAPE};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::cell::TIDOID;
use crate::face::end::LocFrom;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::admit::refusal::Reason;
use crate::rep;

/// The target's junk ctid column and the tid type's facts when the target is hashed; none over the target's own scan.
pub(crate) type Tid = Option<(i16, TypeFacts)>;

/// The oid type (a junk tableoid's).
const OIDOID: u32 = 26;

/// UNION (module doc): the junk tableoid column, the oid type's facts, and each result relation's relid
/// and feed site, in `targets` order; none but under the union form.
pub(crate) type Union<'g> = Option<(i16, TypeFacts, PgVec<'g, (u32, usize)>)>;

/// UNION: the union form's facts for write `w` of pipeline `k` (merge/route.rs `union_targets`, or the
/// union a nest's inner reads, `nest_union`), else none. lockrows UNION-RET: an UPDATE's under RETURNING too (`ret_cells`).
pub(crate) fn union_of<'g>(px: &Px<'g, '_>, k: usize, node: i32, w: &WriteDesc<'_>) -> Union<'g> {
    crate::stack_check();
    union_facts(px, k, node, w)
}

/// UNION-RET (gen lk-0116): an UPDATE of several result relations under RETURNING whose rows come out of one
/// settled union of their scans (write/update.rs's union form). PostgreSQL's ExecProcessReturning projects the row's own
/// result relation's returningList, the first's mapped onto its attnos; the multi-target host hands back the row's own
/// relation's new row (hosts write_route.rs `MultiTargetHost::written`), so the first target's list reads it mapped
/// onto the first's attributes (delete_built.rs `ret_map` / `per_leaf`, D2734's DELETE form), its tableoid the host's.
pub(crate) fn ret_union(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    w.cmd == gravity_program::Cmd::Update && w.returning.is_some() && w.targets.len() > 1 && (crate::ops::modifytable::merge::route::union_targets(gx, k, w).is_some() || nest_union(gx, k, w).is_some() || sorted_outer(gx, k, w).is_some())
}

/// UNION-RET: the site's `n` cells and the row's own relation's cells onto the first target's attributes (delete_built.rs
/// `ret_map`; none when every relation is laid out as the first) under the union form `u`. Refused by name: a list that is
/// not the first target's. D3379 UNION-OLD: OLD's tableoid through a routing root is the replaced version's, its system
/// cells after the site's `n` (ret_sys.rs `union_old`), never the first target's constant.
pub(crate) fn ret_map<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, (w, u, n): (&WriteDesc<'p>, &Union<'_>, usize), node: i32) -> Option<(usize, PgVec<'g, (u32, PgVec<'g, Option<usize>>)>)> {
    u.as_ref()?;
    let p = &gx.pipelines()[k];
    if crate::ops::modifytable::returning::target_at(p, j, w).is_none_or(|t| !core::ptr::eq(t, &w.targets[0])) {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("RETURNING over the union of several result relations: a list that is not the first relation's (lockrows UNION-RET)") });
    }
    super::delete_built::ret_map(gx, w).map(|m| (n, m))
}

/// UNION-RET: `cells` (the site's `n` cells in the row's own relation's layout, then its system cells) as the first
/// result relation's attributes (`per_leaf` by the row's junk tableoid), the system cells after them; empty stays empty.
pub(crate) fn ret_cells<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals, (u, node): (&Union<'_>, i32), (n, map): &(usize, PgVec<'_, (u32, PgVec<'_, Option<usize>>)>), cells: PgVec<'e, (Value, Value)>) -> PgVec<'e, (Value, Value)> {
    if cells.is_empty() {
        return cells;
    }
    let cut = cells.len().min(*n);
    let mut out = super::delete_built::per_leaf(b, env, r, (u, node), map, &cells[..cut]);
    out.extend_from_slice_in(&cells[cut..]);
    out
}

/// D2734 ROWC-UNION: `union_of` under RETURNING too (a DELETE's: returning.rs `make_delete`, whose list is
/// the first result relation's over the row's own cells, delete_built.rs `ret_map`).
pub(crate) fn union_facts<'g>(px: &Px<'g, '_>, k: usize, node: i32, w: &WriteDesc<'_>) -> Union<'g> {
    let feeds = crate::ops::modifytable::merge::route::union_targets(px.gx, k, w).or_else(|| nest_union(px.gx, k, w)).or_else(|| sorted_outer(px.gx, k, w))?;
    let sites = feed_sites(px, &feeds[..], w);
    let (Some(sites), Some(of)) = (sites, typefacts::facts_of(px.gx.facts, OIDOID)) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed write target without its feed's scan site or the oid type" }) };
    Some((w.tableoid_col, of, sites))
}

/// UNION: each result relation's relid and its feed's scan site (`feeds` by target index); `None` when a feed has none.
pub(crate) fn feed_sites<'g>(px: &Px<'g, '_>, feeds: &[usize], w: &WriteDesc<'_>) -> Option<PgVec<'g, (u32, usize)>> {
    let mut v = PgVec::new_in(px.mcx());
    for (&q, t) in feeds.iter().zip(w.targets.iter()) {
        v.push_in((t.relid, px.pipeline_site(q)?));
    }
    Some(v)
}

/// UNION: the row's relation (its junk tableoid) and the recheck code naming that relation's feed site,
/// every feed site's position stored empty first, so a recheck, which reruns every feed, finds no row in the other
/// result relations and the row's own relation's newest version in its own (merge/lower.rs `rel`, MERGE's form). Empty
/// is a valid position at offset 0 (hosts heap / index `window_substituted` read no row); the block word stays, since
/// under a nest (`nest_union`) the feeds are still scanning and a heap window reloads its page's block from the site
/// at every resume (frame heap_pages.rs `next_window`): a zero there turned every later row's ctid into page 0's.
pub(crate) fn union_rel(u: &Union<'_>, node: i32, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals) -> Option<(Value, Value)> {
    let (col, of, sites) = u.as_ref()?;
    let Some(cr) = at.col(*col) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed write's tableoid past the child's projection" }) };
    let mut cx = env.lx(b, node);
    let (rel, _) = rep::datum(&mut cx, cr, of);
    env.memo = cx.memo;
    let mut code = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    for &(relid, s) in sites.iter().rev() {
        let off = site_off(s) as i32;
        // the flags word with the offset (0) beside them, one 32-bit store
        let (flags, r) = (b.iconst(Type::I32, (POS_VALID | POS_ROW) as i64), b.iconst(Type::I32, relid as i32 as i64));
        b.store(env.ctx, off + site::POS_FLAGS as i32, flags);
        b.store(env.ctx, off + site::POS_REL as i32, r);
        let (id, c) = (b.iconst(Type::I64, i64::from(relid)), b.iconst(Type::I64, recheck(s)));
        let mine = b.icmp(IntCond::Eq, rel, id);
        code = b.select(mine, c, code);
    }
    Some((rel, code))
}

/// The write's locator and junk ctid for the producer `built` that scans the hashed target (none: the driving scan's).
pub(crate) fn locate(px: &Px<'_, '_>, node: i32, w: &WriteDesc<'_>, built: Option<usize>) -> (LocFrom, Tid) {
    let Some(q) = built else { return (LocFrom::Driving, None) };
    let (Some(site), col) = (px.pipeline_site(q), w.targets[0].junk_col) else { crate::refuse!(node, Reason::Bug { payload: "a hashed UPDATE target's producer without its scan site" }) };
    let Some(tf) = typefacts::facts_of(px.gx.facts, TIDOID).filter(|_| col > 0) else { crate::refuse!(node, Reason::Bug { payload: "a hashed UPDATE target without its ctid junk column" }) };
    (LocFrom::JunkCtid { site }, Some((col, tf)))
}

/// The target's junk ctid `(datum, valid)` (a NULL ctid: the row op's unsupported), none over its own scan.
pub(crate) fn junk_tid(t: &Tid, node: i32, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals) -> Option<(Value, Value)> {
    let (col, tf) = t.as_ref()?;
    let Some(cr) = at.col(*col) else { crate::refuse!(node, Reason::Bug { payload: "an UPDATE target's ctid past the child's projection" }) };
    let mut cx = env.lx(b, node);
    let (d, null) = rep::datum(&mut cx, cr, tf);
    env.memo = cx.memo;
    let t = b.bconst(true);
    Some((d, b.int_bin(IntBinOp::Xor, null, t)))
}

/// UNION (nest form): PostgreSQL 18.6 plans `UPDATE parent ... FROM other WHERE ..` over an inheritance or
/// partitioned target joined by a nested loop as Nested Loop (the other relation's scan, the Append of the result
/// relations' scans, maybe under a Materialize); decompose settles that Append into a union the nest's inner reads (or
/// whose rows a Material over it spools). EvalPlanQual substitutes the row's result relation's scan with the newest
/// version and blocks the others, so the inner yields that version alone and the join reruns over the other relation's
/// refetched row. The form: pipeline `k`'s ops before its write are filters, gates and projections beside one INNER nest
/// whose inner is the union's emit (the Union op first, then filters and projections), or reads under filters and
/// projections a Material the union's emit spools; the feeds as `union_targets` checks them (one heap range or index scan
/// per result relation under filters and projections). The feeds by target index, else `None`.
pub(crate) fn nest_union<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let p = ps.get(k)?;
    if w.targets.len() < 2 || w.tableoid_col <= 0 {
        return None;
    }
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_)))?;
    let mut nests = p.ops[..wi].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    let (Some((j, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, .. }))), None) = (nests.next(), nests.next()) else { return None };
    // UNION-NEST (inherit#145): the nest's outer may be the settled union of a flattened UNION ALL subquery
    // (`copied_union`), its Union op first
    let lead = copied_union(gx, k, w);
    if p.ops[..wi].iter().enumerate().any(|(i, o)| i != j && !(i == 0 && lead) && !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) {
        return None;
    }
    let ik = ps.inner_of(k, j)?;
    let simple = |ops: &[Op<'_>]| ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    let first = |q: &gravity_decompose::PipelineNode<'_>| q.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    let Source::Breaker { slot } = ps[ik].source else { return None };
    // the union's emit and slot: the inner itself, or the spool filling the Material the inner reads
    let (emit, uslot) = if first(&ps[ik]) && union_of(&ps[ik]).map(|u| u.0) == Some(slot) && simple(&ps[ik].ops[1..]) {
        (ik, slot)
    } else {
        let q = (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot))?;
        let Source::Breaker { slot: u } = ps[q].source else { return None };
        let (last, body) = ps[q].ops.split_last()?;
        if !simple(&ps[ik].ops) || !matches!(last, Op::Join(JoinDesc::Spool)) || !first(&ps[q]) || union_of(&ps[q]).map(|x| x.0) != Some(u) || !simple(&body[1..]) {
            return None;
        }
        (q, u)
    };
    if feed_of(&ps[emit]).is_some() {
        return None;
    }
    // NEST-CLAIM (gen lk-0116): a union pruned at run time by the outer row's parameters, the inner itself
    let feeds = if union_of(&ps[emit]).is_some_and(|u| u.1) { crate::ops::modifytable::merge::nest_claim::embedded_feeds(gx, ik, emit)? } else { crate::rerun::union_join_arms::feeds(gx, emit, uslot) };
    if feeds.len() != w.targets.len() {
        return None;
    }
    let mut by = mcx::pg_vec![in gx.mcx; usize::MAX; w.targets.len()];
    for &f in feeds.iter() {
        let q = &ps[f];
        let (rti, relid) = match &q.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
            // writesubplan (regress subselect#137): or a bitmap heap scan arm (rechecked through its bitmapqualorig)
            Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => (u32::from(*scanrelid), *relid),
            _ => return None,
        };
        let (last, body) = q.ops.split_last()?;
        if feed_of(q) != Some(uslot) || matches!(last, Op::Filter(_) | Op::Project(_)) || !q.desc.consumes.is_empty() || !simple(body) {
            return None;
        }
        let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
        if by[i] != usize::MAX {
            return None;
        }
        by[i] = f;
    }
    Some(by)
}

/// UNION-NEST (inherit#145): pipeline `k` reads, as its source with its Union op first, the settled union
/// of a flattened UNION ALL subquery's members (decompose `WriteDesc::copy_arms`): PostgreSQL 18.6 plans `UPDATE parent
/// ... FROM (SELECT .. UNION ALL SELECT ..) ss WHERE <an OR join qual>` as a Nested Loop whose outer is the Append of the
/// union's arms and whose inner is the Materialize over the Append of the result relations' scans (`nest_union`'s
/// inner). The subquery's COPY mark names a relation no scan carries, so EvalPlanQual scans the members again, whole, and
/// joins them with the row's own relation's newest version; marks.rs reruns those feeds over their whole scans (UNION-COPY,
/// `Marks::rescan`), then the union's emit here reads them all again. Each feed one member's heap range or index scan under
/// filters and projections, no pruning; D3379 (UNION-DRAINED) or an arm kept as a Subquery Scan over an Append of its own
/// (`nested_arm`), beside at least one member's scan.
pub(crate) fn copied_union(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let Some(p) = ps.get(k) else { return false };
    let Source::Breaker { slot } = p.source else { return false };
    let first = p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    if w.copy_arms.is_empty() || !first || union_of(p) != Some((slot, false)) || feed_of(p).is_some() {
        return false;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, slot);
    let member = |q: &gravity_decompose::PipelineNode<'_>| match &q.source {
        Source::Range { am: Am::Heap, scanrelid, .. } | Source::Index { am: Am::Heap, scanrelid, only: false, .. } => w.copy_arms.iter().any(|e| e.1 == u32::from(*scanrelid)),
        _ => false,
    };
    // each feed's last op its Feed (`union_targets`' test), the ops before it filters and projections
    let simple = |q: &gravity_decompose::PipelineNode<'_>| q.ops.split_last().is_some_and(|(last, body)| !matches!(last, Op::Filter(_) | Op::Project(_)) && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    let scanned = |f: usize| member(&ps[f]) && ps[f].desc.consumes.is_empty() && feed_of(&ps[f]) == Some(slot) && simple(&ps[f]);
    feeds.iter().any(|&f| scanned(f)) && feeds.iter().all(|&f| scanned(f) || nested_arm(gx, f, slot).is_some())
}

/// D3379 (UNION-DRAINED, producerchain_union_copy's `SELECT a + 1 FROM some_tab WHERE a > 0` arm): pipeline `f`, a feed of
/// the settled union in `slot`, is an arm PostgreSQL keeps as a Subquery Scan over a Result over an Append of its own (a
/// WHERE over an inheritance parent): the emit of a settled union of its own (its source that union's slot, its Union op
/// first, no pruning), then filters and projections, then its Feed into `slot`; that union's feeds each a heap range or
/// index scan under filters and projections, reading no slot. No mark names those relations, so EvalPlanQual runs the
/// subquery again, whole, as it scans the members (marks.rs `nested_arm_feed` reruns the feeds, the recheck's fixpoint
/// this emit). Its feeds, else `None`.
pub(crate) fn nested_arm(gx: &Gx<'_, '_>, f: usize, slot: u8) -> Option<Vec<usize>> {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let q = ps.get(f)?;
    let Source::Breaker { slot: v } = q.source else { return None };
    let first = q.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    let (last, body) = q.ops.split_last()?;
    let steps = body.get(1..).is_some_and(|b| b.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    if !first || !steps || union_of(q) != Some((v, false)) || feed_of(q) != Some(slot) || matches!(last, Op::Filter(_) | Op::Project(_)) || q.desc.consumes.iter().any(|&s| s != v) {
        return None;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, f, v);
    let scan = |p: &gravity_decompose::PipelineNode<'_>| matches!(p.source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, only: false, .. });
    let simple = |p: &gravity_decompose::PipelineNode<'_>| p.ops.split_last().is_some_and(|(last, body)| !matches!(last, Op::Filter(_) | Op::Project(_)) && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    let fit = |x: usize| x < f && scan(&ps[x]) && ps[x].desc.consumes.is_empty() && feed_of(&ps[x]) == Some(v) && simple(&ps[x]);
    (!feeds.is_empty() && feeds.iter().all(|&x| fit(x))).then_some(feeds)
}

/// D3477 (UNION-COPY, settled): pipeline `k` of an UPDATE / DELETE (or a `settled_merge` MERGE) of one target reads, as
/// its source with its Union op first, the settled union of a flattened UNION ALL subquery whose COPY mark `w` carries
/// (decompose graph `copy_unions`: the Append over its members), its members subqueries, relations or constant rows;
/// the target is hashed by the one producer scanning it (merge/admit.rs `built_target`). The mark names a relation no
/// scan carries, so PostgreSQL's EvalPlanQual scans the members again, whole, and joins them with the target's newest
/// version (an UPDATE takes its values from the first row it joins, a DELETE deletes once): marks.rs reruns every feed
/// (its scan's position cleared, `Marks::rescan`; a constant row made again), then the union's emit here reads them all
/// again. Each feed a heap range, index or bitmap scan or a Result with no child, under filters and projections; a
/// RETURNING list reading the target's columns only. The feeds in arm order, else `None`.
pub(crate) fn copied_settled(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<Vec<usize>> {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let p = ps.get(k)?;
    let Source::Breaker { slot } = p.source else { return None };
    let first = p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    if w.copy_arms.is_empty() || w.merge.as_ref().is_some_and(|m| !settled_merge(gx, w, m)) || w.targets.len() != 1 || !first || union_of(p) != Some((slot, false)) || feed_of(p).is_some() {
        return None;
    }
    // RETURNING reads no other relation's column but the union's own ones its mark's record holds: PostgreSQL projects it
    // over the ORIGINAL plan row (ExecProcessReturning's planSlot), while the recheck's row is the members' re-run one
    if w.returning.is_some_and(|l| !super::ret_cols::outer(l).is_empty()) && ret_record(gx, w).is_none() {
        return None;
    }
    let copy = |rti: u32| w.mark_descs.iter().any(|m| m.rti == rti && m.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY);
    let marked = |node: i32| gx.graph.copy_unions.iter().any(|&(rti, n)| n == node && copy(rti));
    if !p.node_ops.iter().any(|n| n.first_op == 0 && marked(n.node_id)) {
        return None;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, slot);
    let source = |q: &gravity_decompose::PipelineNode<'_>| {
        matches!(q.source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, only: false, .. } | Source::Bitmap { am: Am::Heap, .. } | Source::Const { .. })
    };
    // each feed's last op its Feed, the ops before it filters and projections (`copied_union`'s test)
    let simple = |q: &gravity_decompose::PipelineNode<'_>| q.ops.split_last().is_some_and(|(last, body)| !matches!(last, Op::Filter(_) | Op::Project(_)) && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    let ok = |f: usize| source(&ps[f]) && ps[f].desc.consumes.is_empty() && feed_of(&ps[f]) == Some(slot) && simple(&ps[f]);
    (!feeds.is_empty() && feeds.iter().all(|&f| ok(f))).then_some(feeds)
}

/// D3497: pipeline `k` of an UPDATE / DELETE of one target scans, as its own source, the one live member of a flattened
/// UNION ALL subquery whose COPY mark `w` carries (decompose graph `copy_one`: its quals left that member alone, so no
/// Append carries the mark), a heap range, index or bitmap scan under filters and projections (the member's SubqueryScan
/// among them) up to its first join; the target hashed by the one producer scanning it (merge/admit.rs `built_target`).
/// PostgreSQL's EvalPlanQual hands the mark to no scan and scans the member again, whole, joined with the target's newest
/// version, as `copied_settled`'s union of one feed: the recheck leaves `k`'s own scan unsubstituted (marks.rs
/// `place_rescans`). RETURNING reads the target's columns only.
pub(crate) fn copied_one(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let Some(p) = gx.pipelines().get(k) else { return false };
    if w.copy_arms.is_empty() || w.merge.is_some() || w.targets.len() != 1 || w.returning.is_some_and(|l| !super::ret_cols::outer(l).is_empty()) {
        return false;
    }
    if !matches!(p.source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, only: false, .. } | Source::Bitmap { am: Am::Heap, .. }) {
        return false;
    }
    let copy = |rti: u32| w.mark_descs.iter().any(|m| m.rti == rti && m.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY);
    let marked = |node: i32| gx.graph.copy_one.iter().any(|&(rti, n)| n == node && copy(rti));
    let join = p.ops.iter().position(|o| matches!(o, Op::Join(_) | Op::Write(_))).unwrap_or(p.ops.len());
    let below = p.ops[..join].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    below && p.node_ops.iter().any(|n| marked(n.node_id) && usize::from(n.first_op) <= join)
}

/// D3477 (settled MERGE): a MERGE `copied_settled` takes: WHEN MATCHED actions only, so no join condition is kept and
/// PostgreSQL's ExecMergeMatched calls the row MATCHED when its EvalPlanQual's first re-run row joins the newest version,
/// then acts with the ORIGINAL source row (`context->planSlot`); no RETURNING; and each child column of the union's
/// subquery a plain column of it (decompose graph `copy_union_cols`), which the actions read on the recheck from the mark's
/// stored record (merge/lower.rs `Settled`).
pub(crate) fn settled_merge(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, m: &gravity_decompose::ops::shared::modifytable::MergeDesc<'_>) -> bool {
    let matched = !m.actions.is_empty() && m.actions.iter().all(|a| a.kind == types_nodes::primnodes::MergeMatchKind::MERGE_WHEN_MATCHED);
    let copy = |rti: u32| w.mark_descs.iter().any(|mk| mk.rti == rti && mk.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY);
    let mapped = gx.graph.copy_unions.iter().filter(|e| copy(e.0)).all(|e| !gx.graph.copy_union_cols.iter().any(|c| c.0 == e.0 && c.1 < 0));
    matched && m.join_cond.is_none() && w.returning.is_none() && mapped
}

/// D3496: an UPDATE's or DELETE's RETURNING list of one result relation reading other relations' columns, each one of a union COPY mark's
/// subquery columns its stored record holds (decompose graph `copy_union_cols`): PostgreSQL projects it over the ORIGINAL
/// plan row (ExecProcessReturning's planSlot), the mark's record on a recheck that scans the members again. The mark and
/// each column's resno, record field and type; `None` when the list reads no such column or one the record lacks.
pub(crate) fn ret_record<'g>(gx: &Gx<'g, '_>, w: &WriteDesc<'_>) -> Option<(u32, PgVec<'g, (i16, i16, u32)>)> {
    let outer = super::ret_cols::outer(w.returning?);
    let copy = |rti: u32| w.mark_descs.iter().any(|mk| mk.rti == rti && mk.mark == types_nodes::plannodes::RowMarkType::ROW_MARK_COPY);
    let mut unions = gx.graph.copy_unions.iter().filter(|e| copy(e.0)).map(|e| e.0);
    let rti = unions.next()?;
    if w.merge.is_some() || w.targets.len() != 1 || outer.is_empty() || unions.any(|r| r != rti) {
        return None;
    }
    let field = |r: i16| gx.graph.copy_union_cols.iter().find(|c| c.0 == rti && c.1 == r).map(|c| (r, c.2, c.3));
    let mut cols = mcx::reserved_in(outer.len(), gx.mcx);
    for &r in outer.iter() {
        cols.push_in(field(r)?);
    }
    Some((rti, cols))
}

/// UNION (sorted outer, inherit#134): PostgreSQL 18.6 plans an UPDATE of several result relations over tables
/// never analyzed as a merge join whose outer is the Sort over the Append of the result relations' scans (merge/route.rs
/// `sorted_arms`' form with the sides swapped: the other side's Sort the inner). Decompose streams the arms into that Sort,
/// whose run pipeline `k` reads as its source; a row's result relation is its junk tableoid, and EvalPlanQual substitutes
/// that relation's scan with the newest version (the others empty), re-sorts it and merges it with the other side, rerun
/// (marks.rs: the target arms beside a breaker source, the other side's sorted producers by their marks). The form: `k`'s
/// one join that INNER merge join (no nest, probe or drain), each producer of its source slot one result relation's heap
/// range or index scan under filters and projections and its Append arm, ending in the Sort. The arms by target index.
pub(crate) fn sorted_outer<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps.get(k)?.source else { return None };
    if w.targets.len() < 2 || w.tableoid_col <= 0 || w.merge.is_some() {
        return None;
    }
    let pre = || ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    if pre().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. } | JoinDesc::Probe { .. } | JoinDesc::Drain { .. }))) {
        return None;
    }
    let mut merges = pre().filter(|o| matches!(o, Op::Join(JoinDesc::Merge { .. })));
    let (Some(Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_INNER, .. })), None) = (merges.next(), merges.next()) else { return None };
    let mut by = mcx::pg_vec![in gx.mcx; usize::MAX; w.targets.len()];
    for q in (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces() == Some(slot)) {
        let p = &ps[q];
        let (rti, relid) = match &p.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
            _ => return None,
        };
        let (last, body) = p.ops.split_last()?;
        let arms = body.iter().filter(|o| crate::graph::is_arm(o)).count();
        if !matches!(last, Op::Sort(_)) || arms != 1 || !p.desc.consumes.is_empty() || !body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) || crate::graph::is_arm(o)) {
            return None;
        }
        let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
        if by[i] != usize::MAX {
            return None;
        }
        by[i] = q;
    }
    by.iter().all(|&q| q != usize::MAX).then_some(by)
}

/// D3501 (e2e v5/decorrelate_d2857_write_source: `UPDATE t .. FROM (SELECT *, (SELECT count(*) FROM r WHERE r.ab = s.a ..)
/// cnt FROM s) s WHERE t.a = s.a`, which the planner decorrelates into a Hash Left Join over the grouped
/// `gravity_decorrelated` subquery, a COPY-marked row, above the source probing the hashed target): op `o` of a hashed
/// target's chain (update.rs, delete_built.rs), or D3725 of an UPDATE's chain over its target's own scan (River's
/// JobSchedule), is a LEFT hash probe whose build no scan of a result relation feeds (a written row is never null-extended). Its inner relation's mark positions as under a lock's LEFT join (rerun/epq/marks.rs
/// `sites_in`, D2048 / D3287 (b)): a null-extended row stores the inner's ctid NULL or its COPY row empty, so the recheck
/// null-extends again, as EvalPlanQualFetchRowMark's no-row does; else the recheck reruns the build over the stored row,
/// which the stored source row matches again. PostgreSQL plans the count as a SubPlan per row and evaluates it again over
/// the refetched source row in its recheck: the same count.
pub(crate) fn left_built(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, o: &Op<'_>) -> bool {
    fn deep(p: &gravity_decompose::PipelineNode<'_>, f: &dyn Fn(&Source<'_>) -> bool) -> bool {
        crate::stack_check(); // D3832
        f(&p.source) || p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if deep(inner, f)))
    }
    let Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, slot, .. }) = o else { return false };
    let ps = gx.pipelines();
    let Some(b) = gx.life_of(k, *slot).map(|l| usize::from(l.0)).filter(|&b| b < ps.len()) else { return false };
    let written = |s: &Source<'_>| matches!(s, Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } | Source::Tid { scanrelid, .. } if u32::from(*scanrelid) == w.rti || w.targets.iter().any(|t| t.rti == u32::from(*scanrelid)));
    gx.with_producers([b]).iter().all(|&q| !deep(&ps[q], &written))
}
