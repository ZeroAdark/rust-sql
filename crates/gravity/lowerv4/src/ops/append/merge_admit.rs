//! Props: MergeAppend's graph facts and its admission predicate (runs/mergeappend.rs; one predicate shared by props and the
//! kind's file).
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::sort::{SortForm, SortKey};
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_abi::breaker::BreakerKind;
use mcx::{CollectIn, PgVec};
use types_nodes::Node;
use types_nodes::list::NodeList;

use crate::effects::visible::merge::{builtin_key, pure, pure_in};
use crate::graph::Gx;

/// D449's raise-free reading and its rule over a pipeline's ops are the effects home's (effects/visible/merge.rs); props
/// names them (lib.rs).
pub(crate) use crate::effects::visible::merge::{ops_can_raise, pure_given};

/// One MergeAppend as the graph holds it: its slot, keys, reader pipeline and arm pipelines (graph order).
pub(crate) struct Merge<'g> {
    pub(crate) slot: u8,
    pub(crate) keys: &'g [SortKey],
    pub(crate) reader: usize,
    pub(crate) arms: PgVec<'g, usize>,
}

/// An arm's Feed slot.
fn feed_slot(p: &PipelineNode<'_>) -> Option<u8> {
    p.ops.iter().find_map(|op| if let Op::Append(AppendDesc::Feed { slot, .. }) = op { Some(*slot) } else { None })
}

/// A reader's Merge op: (slot, keys, children).
fn merge_op<'g>(p: &'g PipelineNode<'_>) -> Option<(u8, &'g [SortKey], u16)> {
    p.ops.iter().find_map(|op| if let Op::Append(AppendDesc::Merge { slot, keys, children, .. }) = op { Some((*slot, &keys[..], *children)) } else { None })
}

impl<'g> Merge<'g> {
    /// The MergeAppend whose reader is pipeline `j`: its arms are the last `children` pipelines before `j` that feed the
    /// slot (decompose reuses a slot after its reader, so the walk stops at the count).
    pub(crate) fn of_reader(gx: &Gx<'g, '_>, j: usize) -> Option<Merge<'g>> {
        let ps = gx.pipelines();
        let (slot, keys, n) = merge_op(&ps[j])?;
        let feeds = |q: &PipelineNode<'_>| matches!(crate::frame::iface::breaker_of(&q.desc), Some(b) if b.slot == slot) && feed_slot(q) == Some(slot);
        let mut arms: PgVec<'g, usize> = (0..j).rev().filter(|&q| feeds(&ps[q])).take(n as usize).collect_in(gx.mcx);
        arms.reverse();
        (arms.len() == n as usize).then_some(Merge { slot, keys, reader: j, arms })
    }

    /// The MergeAppend arm pipeline `k` feeds: the first later pipeline holding the slot's Merge op, an embedded inner's
    /// too (D1483: a SubPlan's or nest inner's MergeAppend, regress inherit#759; inners index past the graph pipelines).
    pub(crate) fn of_arm(gx: &Gx<'g, '_>, k: usize) -> Option<Merge<'g>> {
        let slot = feed_slot(&gx.pipelines()[k])?;
        let j = (k + 1..gx.pipelines().end()).find(|&j| merge_op(&gx.pipelines()[j]).is_some_and(|m| m.0 == slot))?;
        Merge::of_reader(gx, j).filter(|m| m.arms.contains(&k))
    }
}

/// Admission of MergeAppend `m`: every arm reads all of its rows into the slot, where PostgreSQL's merge stops pulling
/// once its reader stops. That is the same set of rows when nothing stops the reader early (`reads_all`: no InitPlan);
/// otherwise no arm's row may raise (error-vs-value, D449).
pub(crate) fn arms_admit(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let stops = !gx.graph.init_plans.is_empty() || !reads_all(gx, m.reader, true);
    if !stops {
        return true;
    }
    let ps = gx.pipelines();
    let mut aggs = false;
    let quiet = m.arms.iter().all(|&a| {
        let p = &ps[a];
        let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(p.ops.len());
        !ops_can_raise(gx, &p.ops[..f]) || {
            let ok = arm_counts_admit(gx, a, &p.ops[..f]);
            aggs |= ok;
            ok
        }
    });
    if quiet {
        // shapecheck fire signal (REGISTRY OID-29, OIDW-10, OIDW-42..46): a stopping reader's arms read whole, raise-free
        fires::fire(if aggs { "merge.agg_arms" } else { "merge.quiet_arms" });
        return true;
    }
    pulls_once(gx, m) && crate::effects::visible::merge::first_row(gx, m)
}

/// D2733: merge `m` is admitted (`arms_admit`) because its reader reads every row while an arm row can raise
/// (`ops_can_raise`). That read is PostgreSQL's only when the statement runs to its end: a cursor's FETCH, a protocol
/// portal's row limit, SPI or a SQL function's lazy row may stop PostgreSQL's merge after a few rows, where the breaker
/// form has read every arm whole and raised (FETCH 3 over `WHERE k / (k - 1501) > -100 ORDER BY k`: PG's three rows, a
/// division by zero). Its program is then served only to a reader that takes every row (merge.rs `make_end`:
/// `Px::whole_read`, D1922's portal-kind fact). Not where a pipeline between the merge and the portal folds all of its
/// input before its first row in PostgreSQL too (`folds_whole`): then the portal's first row reads the merge whole and
/// the stop comes after it (K10.y1 / y1e / y1m .f1.h0: a cursor over an aggregate or a sort over a grouping-sets merge).
pub(crate) fn whole_only(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    let reads = gx.graph.init_plans.is_empty() && reads_all(gx, m.reader, true) && !folds_whole(gx, m.reader);
    reads && m.arms.iter().any(|&a| {
        let p = &ps[a];
        let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(p.ops.len());
        ops_can_raise(gx, &p.ops[..f])
    })
}

/// D2733: whether a pipeline on the slot-reader chain from `k` to the root (`reads_all`'s chain)
/// ends in a breaker PostgreSQL's executor also runs to its end before its first row: a full sort, a plain aggregate, or
/// a hashed group (HashAggregate). A sorted group (GroupAggregate), a Group or Unique node and a sorted DISTINCT hand
/// each group on as its run ends, so a cursor's FETCH stops them, and the merge under them, early: not a fold.
fn folds_whole(gx: &Gx<'_, '_>, k: usize) -> bool {
    use crate::face::kinds::KindId;
    let ps = gx.pipelines();
    let root = gx.graph.root as usize;
    let mut q = k;
    while q != root {
        // the end's kind (graph.rs `end_need`): a plain fold, a hashed or two-pass keyed group outside grouping sets
        // (PostgreSQL's MixedAggregate hands its sorted sets on as it reads), a full sort's run (no presorted prefix: an
        // Incremental Sort hands on each prefix group); a sorted group is `runs.group` / `runs.unique`
        let ungrouped_sets = || gx.group_at(q).is_some_and(|g| g.set.is_none());
        let fold = match gx.end_need(q) {
            KindId::FoldPlain => true,
            KindId::KeyedHashagg | KindId::KeyedTwopass | KindId::KeyedDistinctPlain => ungrouped_sets(),
            KindId::RunsSort => ps[q].ops.iter().any(|op| matches!(op, Op::Sort(d) if matches!(d.form, SortForm::Runs) && d.presorted == 0)),
            _ => false,
        };
        if fold {
            return true;
        }
        match crate::rerun::union_arms::reader(gx, q) {
            Some(c) => q = c,
            None => return false,
        }
    }
    false
}

/// V4R-465 (the MinMax InitPlan's merge, inherit#727 / #728): under a LIMIT 1 (offset none or 0), its reader's one bound,
/// in the root's or a non-CTE InitPlan's pipeline, PostgreSQL's merge pulls each arm once in arm order (a Sort arm reads
/// its input whole) and never again; no pruning. Whether its arms may raise there is the effects home's
/// (effects/visible/merge.rs `first_row`).
fn pulls_once(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    let r = &ps[m.reader];
    let limit = |op: &Op<'_>| crate::frame::iface::limit_bounds(op).is_some();
    let lone = r.ops.iter().all(|op| matches!(op, Op::Project(_) | Op::Append(AppendDesc::Merge { prune: None, .. })) || limit(op))
        && r.ops.iter().filter(|&op| limit(op)).count() == 1;
    let one = matches!(merge_cap(gx, m), Some((Bound::Const(1), Bound::All | Bound::Const(0))));
    let init = gx.graph.init_plans.iter().any(|i| i.pipeline as usize == m.reader && !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte));
    lone && one && (m.reader == gx.graph.root as usize || init)
}

/// V4R-465: pipeline `q` is a full sort over a scan into `slot`, read by arm `a` alone: filters, projections and one sort
/// (every row, no top or presorted prefix) over a relation, its keys builtin scalar orders.
pub(crate) fn full_sort(gx: &Gx<'_, '_>, q: usize, slot: u8, a: usize) -> bool {
    let ps = gx.pipelines();
    let p = &ps[q];
    let sorts = |op: &Op<'_>| matches!(op, Op::Sort(d) if matches!(d.form, SortForm::Runs) && d.presorted == 0 && d.keys.iter().all(|k| builtin_key(gx, k, d.tlist)));
    let ops = p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)) || sorts(op)) && p.ops.iter().filter(|op| matches!(op, Op::Sort(_))).count() == 1;
    let alone = (0..ps.len()).all(|c| c == a || !ps[c].desc.consumes.contains(&slot) || gx.life_of(c, slot).is_some_and(|l| l.0 as usize != q));
    let end = matches!(crate::frame::iface::breaker_of(&p.desc), Some(b) if b.slot == slot && matches!(b.kind, BreakerKind::Sort));
    end && ops && alone && p.source.relation().is_some() && p.desc.consumes.is_empty()
}

/// D1486 (material, foreign_key#1393): every key of MergeAppend `m` is a builtin scalar order (`builtin_key` over its last
/// arm's row), so its compares neither act nor raise (rerun/union_join_arms.rs `merged`).
pub(crate) fn builtin_keys(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    m.arms.last().and_then(|&a| arm_cols(&ps[a])).is_some_and(|cols| m.keys.iter().all(|k| builtin_key(gx, k, cols)))
}

/// An arm's Feed op's position (the ops before it are the arm's own).
pub(crate) fn feed_at(p: &PipelineNode<'_>) -> usize {
    p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(p.ops.len())
}

/// An arm's row: its last projection before the Feed.
pub(crate) fn arm_cols<'g, 'p>(p: &'g PipelineNode<'p>) -> Option<&'p NodeList<'p>> {
    p.ops[..feed_at(p)].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })
}

/// Whether PostgreSQL reads every row pipeline `k` emits: the root with no LIMIT or SubPlan (above a breaker, no gate and
/// no LIMIT that may hand on no row), or
/// (V4R-466) a pipeline of
/// filters, projections and a full sort (no LIMIT, SubPlan, gate, join, window, set operation or write) ending in a breaker that folds
/// all of its input before its first row (an aggregate, a full sort, a DISTINCT; never a top-k, a hash build its join may
/// skip, or a Materialize), whose reader, scanning the slot, reads every row the same way.
fn reads_all(gx: &Gx<'_, '_>, k: usize, first: bool) -> bool {
    crate::stack_check();
    let (ps, p) = (gx.pipelines(), &gx.pipelines()[k]);
    if k == gx.graph.root as usize {
        // above a breaker, a gate too: a false one-time qual never pulls the breaker's input. D449: a LIMIT above a breaker
        // that folds its input whole pulls it at its first row unless its count may be 0 or its bounds raise (nodeLimit.c)
        let limit = if first { crate::frame::iface::holds_limit(&p.ops) } else { crate::frame::iface::limit_may_skip(&p.ops) };
        // D4250 (D3365's stop): a merge join over the merged rows that fills no unmatched outer row stops PostgreSQL's
        // pulls of them once its inner ends (`merge_outer_stops`); a merge above a folding breaker (`!first`) reads it whole
        // before its first row whatever stops later
        let joined = first && merge_outer_stops(p, false);
        return !limit && !joined && !p.ops.iter().any(|op| matches!(op, Op::SubPlan(_)) || !first && matches!(op, Op::Gate(_)));
    }
    let ops_ok = p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Append(AppendDesc::Merge { .. }) | Op::FirstOfRun(_) | Op::GroupRun(_))
        || matches!(op, Op::Sort(d) if matches!(d.form, SortForm::Runs) && d.presorted == 0)); // a full sort's own op
    let Some(b) = crate::frame::iface::breaker_of(&p.desc) else { return false };
    let full = matches!(b.kind, BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Sort | BreakerKind::Distinct);
    // the slot's reader: the first pipeline after `k` consuming it (decompose reuses a slot only after its reader)
    let reader = (k + 1..ps.len()).find(|&q| ps[q].desc.consumes.contains(&b.slot));
    ops_ok && full && reader.is_some_and(|q| matches!(ps[q].source, Source::Breaker { slot } if slot == b.slot) && reads_all(gx, q, false))
}

/// D4250 (D3365's stop over a MergeAppend): reader `p`'s ops after its Merge op hand the merged rows to a merge join as
/// that join's outer, of a type that fills no unmatched outer row (INNER, SEMI, RIGHT, RIGHT ANTI): PostgreSQL's merge
/// fetches no more of its outer once its inner ends (nodeMergejoin.c, unless mj_FillOuter), so it never pulls the arm
/// rows past that point. `plain`: only filters and projections between (the pull form's stop is the merge's own DONE).
fn merge_outer_stops(p: &PipelineNode<'_>, plain: bool) -> bool {
    use gravity_decompose::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    let Some(at) = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. }))) else { return false };
    let stops = |op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Merge { jointype, .. }) if matches!(*jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI));
    match plain {
        true => p.ops[at + 1..].iter().find(|op| !matches!(op, Op::Filter(_) | Op::Project(_))).is_some_and(stops),
        false => p.ops[at + 1..].iter().any(stops),
    }
}

/// D1417: arm `a` (its ops before the Feed) over a group emit whose aggregates are raise-free (effects/visible/merge.rs
/// `raise_free_agg`). PostgreSQL's sorted Agg folds only the groups its merge pulls, where the group slot's folds read every
/// row: each fold pipeline (the slot's producers since its previous reader) scans a relation, reads no other breaker, and
/// raises on no row.
fn arm_counts_admit(gx: &Gx<'_, '_>, a: usize, ops: &[Op<'_>]) -> bool {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[a].source else { return false };
    let arm_ok = ops.iter().all(|op| match op {
        Op::Project(l) => l.iter().all(|n| pure_in(gx, n, true, &[])),
        Op::Filter(q) => pure(gx, *q),
        _ => true,
    });
    let start = (0..a).rev().find(|&q| ps[q].desc.consumes.contains(&slot)).map_or(0, |q| q + 1);
    let folds: PgVec<'_, usize> = (start..a).filter(|&q| matches!(crate::frame::iface::breaker_of(&ps[q].desc), Some(b) if b.slot == slot)).collect_in(gx.mcx);
    let fold_ok = |q: usize| matches!(ps[q].source, Source::Range { .. }) && ps[q].desc.consumes.is_empty() && !ops_can_raise(gx, &ps[q].ops);
    arm_ok && !folds.is_empty() && folds.iter().all(|&q| fold_ok(q))
}

/// Admission of a bounded incremental sort (a top whose sort has a presorted prefix) in pipeline `k`: the top reads every
/// row where PostgreSQL stops at the closing prefix group, so no row before the sort may raise (D449). Read by
/// runs/topk_rows.rs (M3-TOPS) for `SortForm::TopK` with `presorted > 0` where its stop (V4R-179: PostgreSQL's rows exactly)
/// does not apply: WITH TIES, a parallel producer, a prefix key that is not one exact order word.
pub(crate) fn incremental_admit(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let s = p.ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(p.ops.len());
    !ops_can_raise(gx, &p.ops[..s])
}

/// The arm cap's bounds (runs/mergeappend.rs `Cap`): the reader's LIMIT `(count, offset)` when only projections run between
/// the Merge op and it, and it has a count, no WITH TIES and no bound over executor parameters (D1016 row 5: the Limit step
/// reads it per open, `Cap` has no word for it); then the merge emits at most `count + offset` rows.
pub(crate) fn merge_cap<'p>(gx: &Gx<'_, 'p>, m: &Merge<'_>) -> Option<(Bound<'p>, Bound<'p>)> {
    use crate::frame::iface::{limit_bounds, limit_with_ties};
    use crate::rerun::params_bound::computed;
    let r = &gx.pipelines()[m.reader];
    let at = r.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. })))?;
    for op in r.ops[at + 1..].iter() {
        if matches!(op, Op::Project(_)) {
            continue;
        }
        let (count, offset) = limit_bounds(op)?;
        return (!computed(count) && !computed(offset) && !limit_with_ties(op) && !matches!(count, Bound::All)).then_some((*count, *offset));
    }
    None
}

/// A stop of the pull form's reader `r` (`after`: its ops past the Merge op) that ends its walk at PostgreSQL's last row,
/// so the pull form, which reads PostgreSQL's arm rows (a row per arm ahead, then the emitting arm's next), needs no
/// `arms_admit` under it: with only projections between, a LIMIT WITH TIES with a count over no executor parameter
/// (limit/lower.rs: past the count, the first row whose tie keys differ ends the walk, the row nodeLimit.c reads last),
/// or a counted top whose stop is exact (ops/shared/sort.rs `exact_stop`).
fn exact_reader_stop(gx: &Gx<'_, '_>, r: usize, after: &[Op<'_>]) -> bool {
    use crate::rerun::params_bound::computed;
    for op in after {
        if let Some((count, offset)) = crate::frame::iface::limit_bounds(op) {
            return crate::frame::iface::limit_with_ties(op) && !matches!(count, Bound::All) && !computed(count) && !computed(offset);
        }
        match op {
            Op::Project(_) => {}
            Op::Sort(_) => return crate::ops::shared::sort::exact_stop(gx, r),
            _ => return false,
        }
    }
    false
}

/// The pull form's frame words (runs/merge_pull.rs: per arm its flags, columns, pairs and copy buffers; the heap's,
/// merge_pull_heap.rs, a word per arm) stay within a quarter of the frame's word index space (frame/px.rs `frame_word`, a
/// `u16`), the rest left to the program's other words. A format limit, not an arm count: any arm count whose words fit
/// (RUNS-35 / STEP-25 / CAP-02; was 32 arms by name, CNM-66).
pub(crate) const PULL_WORDS: usize = (u16::MAX as usize + 1) / 4;

/// The words the bound reserves past the arms' for `arms` arms: those of the 4-wide tournament the pull form picked through
/// before D2884's heap (each node a tag, the pairs and the columns), more than the heap takes, so the admitted set stays.
fn reserved_nodes(arms: usize) -> usize {
    let (mut count, mut all) = (arms, 0);
    while count > 4 {
        count = count.div_ceil(4);
        all += count;
    }
    all
}

/// Merge `m` in the pull form (runs/merge_pull.rs, V4R-143, D478): under the reader's LIMIT (`merge_cap`), or under a
/// counted top over a presorted prefix whose walk stops itself (decompose facts/stream.rs `top_stops`: an Incremental
/// Sort under a LIMIT, group's stop-early road), or under row-at-a-time ops and a counted LIMIT (`run_stops`: a UNION's
/// or DISTINCT's Unique, a streamed GROUP BY, a nest or merge join outer; ci-0131, rnd-0930, PSP3-01), these two also
/// where an arm row may raise past the stop (D449: the breaker form reads past it); or under a reader stop at PostgreSQL's last
/// row (`exact_reader_stop`: a LIMIT WITH TIES, a top whose stop is exact) whatever its arm rows may raise; any arm count
/// whose frame words fit (`PULL_WORDS`),
/// each a heap index scan or a full sort's run this arm alone reads (`full_sort`) whose ops before its Feed are
/// filters and projections, every merge key a
/// by-value type the catalog knows (its datum is one word), a text / varchar (V4R-458) or a comparator-ordered varlena
/// with a builtin comparator (a numeric, an inet: its held copy compared past its prefix word), every other column by
/// value, a varlena (V4R-305) or a fixed-width by-reference image (a name: its copy held as a varlena's). It reads PostgreSQL's rows (a row per arm ahead, then the
/// emitting arm's next), so `arms_admit`'s D449 rule does not apply to it (M4-EXIT F5). D449 (partition_prune#794): it
/// also serves a merge with no stop of its own whose arm rows can raise and call user code or a VOLATILE function:
/// the breaker form has no exact road there (D449's rule, and errorhold never runs such a call ahead,
/// D2573), where the pull form makes PostgreSQL's calls in PostgreSQL's order. D2884: a consumer that shows a tie's member
/// (`tie_shown`) over index arms where the pull form reads PostgreSQL's rows (`tie_reads_pg`): its heap emits PostgreSQL's order on ties.
pub(crate) fn merge_pull(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    let arm = |a: usize| {
        let p = &ps[a];
        let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(0);
        // a Sort arm, its producer a full sort this arm alone reads (Merge Append over an index and a Sort): the reader pulls the run
        // as a cursor (runs/merge_pull.rs `RunArm`), as PostgreSQL's Sort node reads its input whole and then hands rows on
        let src = match p.source {
            gravity_decompose::Source::Index { am: gravity_decompose::Am::Heap, .. } => true,
            // fail-closed until its corners are witnessed: no pruning (a pruned arm's sort would still run) and a reader no
            // rerun set re-opens (its producer would not re-run under it)
            gravity_decompose::Source::Breaker { slot } => {
                crate::ops::append::merge::prune_of(gx, m.reader).is_none()
                    && !crate::rerun::sets::in_owner_set(gx, m.reader)
                    && gx.life_of(a, slot).is_some_and(|l| full_sort(gx, l.0 as usize, slot, a))
                    // the run's slot is held through the reader (merge_pull_runs.rs): no pipeline from the arm to the
                    // reader names its number (decompose frees a slot at its life's end, the arm, and may reuse it)
                    && (a + 1..=m.reader).all(|q| ps[q].desc.produces() != Some(slot) && !ps[q].desc.consumes.contains(&slot))
            }
            _ => false,
        };
        src && p.ops[..f].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
    };
    let Some(last) = m.arms.last() else { return false };
    let p = &ps[*last];
    let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(0);
    let Some(cols) = p.ops[..f].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else { return false };
    // each column's datum word: by value, or a varlena (a non-key column, whose held copy runs/merge_pull_copy.rs keeps)
    let words = cols.iter().enumerate().map(|(c, n)| {
        let te = n.as_target_entry()?;
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        let tf = gravity_decompose::catalog::typefacts::facts_of(gx.facts, typ)?;
        let key = m.keys.iter().any(|k| k.resno as usize == c + 1);
        // V4R-458: a text / varchar key (not bpchar: its trim) orders past its prefix word by runs/merge_pull_cmp.rs
        let text_key = key && tf.typlen == -1 && tf.image.is_plain_text();
        // a comparator-ordered varlena key (runs/keylane.rs `hosted`: a numeric no packed lane holds, an inet / cidr, an
        // array or a range): past its inexact prefix word its btree comparator decides over the held copies
        // (merge_pull_cmp.rs), never one that may reach user code (`rep::user_reach`: a composite, a user type)
        let hosted_key = key && tf.typlen == -1 && crate::rep::keylane::hosted(&tf) && !crate::rep::user_reach(gx.catalog, tf.base);
        // a fixed-width by-reference non-key column (a name, a uuid, an interval): its image is copied as a varlena's is
        let fixed = !key && !tf.byval && tf.typlen > 0;
        (tf.byval && !(key && tf.image.is_enum()) || !key && tf.typlen == -1 || text_key || hosted_key || fixed).then_some(!tf.byval) // an enum key: its comparator's tail (V4R-386)
    }).try_fold(0usize, |t, w| w.map(|x| t + usize::from(x)));
    // the reader's frame words per arm (`i` a byte: runs/merge_pull.rs) bound the row: 64 columns, 32 keys, and
    // 2 + 2 * (columns + keys + varlenas) words
    let fits = |v: usize| cols.len() <= 64 && m.keys.len() <= 32 && cols.len() + m.keys.len() + v <= 127;
    let per_arm = |v: usize| 2 + 2 * (cols.len() + m.keys.len() + v);
    let words_fit = |v: usize| m.arms.len() * per_arm(v) + reserved_nodes(m.arms.len()) * (2 + 2 * (m.keys.len() + cols.len())) <= PULL_WORDS;
    let r = &ps[m.reader];
    let after = r.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. }))).map_or(&[][..], |at| &r.ops[at + 1..]);
    // D449 (partition_prune#794): an arm that calls user code or a VOLATILE function and whose rows can raise has no
    // breaker form (`arms_admit` fails, and errorhold never runs such a call ahead: frame/px.rs `run_ahead`, D2573), so
    // the pull form takes it with no stop of its own; every merge the breaker form serves keeps it
    let calls = |n: Node<'_>| crate::effects::user_or_volatile(gx, n);
    let user = |a: usize| ps[a].ops.iter().any(|op| match op {
        Op::Filter(x) => calls(*x),
        Op::Project(l) => l.iter().any(calls),
        _ => false,
    });
    // C1 part 2: the reader is the statement's root pipeline under a partial reader (decompose `PipelineGraph::partial`, a
    // named cursor's FETCH): its sink stops where each FETCH stops pulling, as a counted LIMIT at the end of its ops would
    let cursor = gx.graph.partial && m.reader == gx.graph.root as usize;
    let early = gravity_decompose::facts::stream::top_stops(after) || run_stops(after, m.arms.len(), cursor);
    // D2884: a consumer that shows which member of a run of keys equal across arms came first takes the pull form, whose
    // heap (merge_pull_heap.rs) shows PostgreSQL's member (the breaker form's arm order serves elsewhere: a tie choice),
    // over index arms (a Sort arm's own ties are its sort's) up to the arm count where the pull form was measured the
    // cheaper whole read too
    let index = m.arms.iter().all(|&a| matches!(ps[a].source, gravity_decompose::Source::Index { .. }));
    // ... where the pull form reads PostgreSQL's rows (`tie_reads_pg`)
    let ties = index && m.arms.len() <= gravity_decompose::ops::shared::append::PULL_WHOLE_ARMS && tie_shown(gx, m) && tie_reads_pg(gx, m);
    // D2922 (R1): a sorted group streamed right after the merge that folds its raising calls in plan order (decompose
    // facts/stream.rs `in_order` over `merge_spine`) must read PostgreSQL's rows, ties across arms too, which only this
    // form reads; its group stops where PostgreSQL's does (a LIMIT, or a top's prefix stop), so the pull reads no row past
    let ordered = after.iter().find(|op| !matches!(op, Op::Project(_))).is_some_and(|op| matches!(op, Op::GroupRun(i) if gx.stream(*i as usize).is_some_and(|g| gx.graph.in_order.contains(&g.node_id))));
    // D449: under an early stop an arm row that may raise past it takes this form too, where the breaker form reads past
    // the stop (it refuses there without errorhold's licence: a plan-order re-run, a statement that acts)
    // D4250: or the merged rows go, through filters and projections, into a merge join's outer that PostgreSQL stops
    // fetching once its inner ends (`merge_outer_stops`), merge.join's DONE ending this walk there (`Env::full`), while an
    // arm row may raise past that stop: the pull form reads PostgreSQL's rows where the breaker form reads every arm whole
    let joined = merge_outer_stops(r, true) && m.arms.iter().any(|&a| ops_can_raise(gx, &ps[a].ops[..feed_at(&ps[a])]));
    let stops = merge_cap(gx, m).is_some() || exact_reader_stop(gx, m.reader, after) || early || (!arms_admit(gx, m) && m.arms.iter().any(|&a| user(a))) || ties || ordered || joined;
    // USEROP: a key of another family's order (decompose append.rs `merge_keys`: the CompareFn key kind) has no word or
    // prefix order the pull form compares; the breaker form's runs order it by its comparator (ops/sort runs.sort)
    let words_order = m.keys.iter().all(|k| !k.own_family);
    // A Sort arm's producer runs whole before the reader, where PostgreSQL's Sort reads its input at the merge's first pull
    // of that arm (arm order, after an earlier arm's first row) and never when nothing pulls the merge (an outer LIMIT 0):
    // that order is unseen when no Sort input can raise or call user code or a VOLATILE function, whatever the arms call
    // (their rows are pulled in PostgreSQL's order; a quiet input's early run shows nothing: append_leakproof_builtin's
    // user int4div in an index arm beside a Sort arm), where a raising one shows it (m9_d449_sort_arm: an index arm's
    // division by zero before the Sort input's overflow, a nextval in a Sort input). D449: or when no arm and no Sort
    // input calls user code or a VOLATILE function and each Sort input that may raise holds its error (op 14 for
    // runs.sort, frame/iface/sort.rs `held_fill`) until the reader first reads its run's end (merge_pull.rs `fill`),
    // PostgreSQL's first pull of that arm
    let quiet = |q: usize| !ops_can_raise(gx, &ps[q].ops) && !user(q);
    let plain = m.arms.iter().all(|&a| !user(a));
    let unseen = |a: usize, q: usize| quiet(q) || plain && !user(q) && crate::frame::iface::held_fill(gx, a, q);
    let runs_unseen = m.arms.iter().filter_map(|&a| match ps[a].source {
        gravity_decompose::Source::Breaker { slot } => Some((a, gx.life_of(a, slot).map(|l| l.0 as usize))),
        _ => None,
    }).all(|(a, q)| q.is_some_and(|q| unseen(a, q)));
    stops && words_order && runs_unseen && m.arms.iter().all(|&a| arm(a)) && words.is_some_and(|v| fits(v) && words_fit(v))
}

/// The stop road over a merge: `ops`, the reader's after its Merge op, hand each merge row on before the walk reads the
/// next, then a LIMIT with a count (an OFFSET too) and no WITH TIES ends the walk, so the merge is pulled only for the rows
/// those ops ask for. The first-of-run road (a UNION's or DISTINCT's Unique: projections, one first-of-run op, projections
/// or filters; decompose facts/stream.rs `limit_streams`, V4R-147) at any arm count. PSP3-01: a sorted group streamed one
/// group at a time (D1071's `Op::GroupRun`: a group leaves as the next group's first row comes), a filter before the
/// first-of-run op (a HAVING), and a join whose outer is the merge's rows (a nested loop, a merge join outer) may read
/// every merge row before the LIMIT is met, so over at most `PULL_WHOLE_ARMS` arms, where the pull form's pick costs no
/// more than the breaker form's run merge. A WindowAgg's buffer, a full sort or a hash build reads its input whole and is
/// no op of this pipeline: such a reader keeps the breaker form (a window: decompose ops/window.rs `cap_merge`). C1 part 2:
/// the same ops ending at the root of a partial reader (`cursor`: a named cursor, each FETCH a count) stop there too.
fn run_stops(ops: &[Op<'_>], arms: usize, cursor: bool) -> bool {
    use crate::frame::iface::{limit_bounds, limit_with_ties};
    use gravity_decompose::join::JoinDesc;
    let (mut run, mut wide) = (false, false);
    for op in ops {
        if let Some((count, _)) = limit_bounds(op) {
            let counted = !matches!(count, Bound::All) && !limit_with_ties(op);
            return counted && (run && !wide || arms <= gravity_decompose::ops::shared::append::PULL_WHOLE_ARMS);
        }
        match op {
            Op::Project(_) => {}
            Op::FirstOfRun(_) => (wide, run) = (wide || run, true),
            Op::Filter(_) => wide |= !run,
            Op::GroupRun(_) | Op::Join(JoinDesc::Nest { .. } | JoinDesc::Merge { .. }) => wide = true,
            _ => return false,
        }
    }
    cursor && (run && !wide || arms <= gravity_decompose::ops::shared::append::PULL_WHOLE_ARMS)
}

/// D2884: the pull form reads PostgreSQL's rows under merge `m`, so a consumer that shows a tie's member takes it
/// (`merge_pull`) and shows PostgreSQL's member: nothing past the reader stops before its input ends (`chain_halts`),
/// and either the reader stops the pull where PostgreSQL's reader stops it (`merge_cap`, `exact_reader_stop`, a counted
/// top or the stop road), or the reader reads every merge row (its ops past the Merge op stream every row on: `halts`)
/// and no arm row can raise or call user code or a VOLATILE function, so that a reader PostgreSQL stops from outside (a
/// cursor's FETCH) reads no row that shows. Else the merge keeps the breaker form, whose run merge emits a tie in arm
/// order (any member of a run of equal keys is a right answer): a window under a LIMIT whose cap does not reach the
/// merge (lag, lead, first_value; more than `PULL_FEW_ROWS` rows) read every arm whole pulled, 64-70 ms against the row
/// executor's 1 ms.
fn tie_reads_pg(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    let ps = gx.pipelines();
    let r = &ps[m.reader];
    let after = r.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. }))).map_or(&[][..], |at| &r.ops[at + 1..]);
    if chain_halts(gx, m.reader) {
        return false;
    }
    // C1 part 2: a partial reader's stop (`run_stops`'s `cursor`) is not counted here: a cursor over arm rows that can
    // raise keeps the breaker form (its arm order on ties)
    let own = merge_cap(gx, m).is_some() || exact_reader_stop(gx, m.reader, after) || gravity_decompose::facts::stream::top_stops(after) || run_stops(after, m.arms.len(), false);
    let quiet = m.arms.iter().all(|&a| {
        let p = &ps[a];
        let f = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or(p.ops.len());
        !ops_can_raise(gx, &p.ops[..f]) && !p.ops[..f].iter().any(|op| match op {
            Op::Filter(x) => crate::effects::user_or_volatile(gx, *x),
            Op::Project(l) => l.iter().any(|n| crate::effects::user_or_volatile(gx, n)),
            _ => false,
        })
    });
    own || quiet && !halts(after)
}

/// D2884: whether ops `ops` may end their pipeline's walk before its input ends: any op but a filter, a projection, a
/// first-of-run op, a streamed group, a set-returning function, a per-row SubPlan, a Sort or a window with no run
/// condition (a LIMIT, a run condition, a join whose other side may end first, a one-time gate, ...).
fn halts(ops: &[Op<'_>]) -> bool {
    !ops.iter().all(|op| match op {
        Op::Filter(_) | Op::Project(_) | Op::FirstOfRun(_) | Op::GroupRun(_) | Op::Srf(_) | Op::SubPlan(_) | Op::Sort(_) => true,
        Op::Window(d) => d.run_condition.is_nil(),
        _ => false,
    })
}

/// D2884: whether a pipeline that reader `r`'s rows reach through its breaker, each in turn to the client's sink, may stop
/// reading them early (`halts`), or the breaker is read by more than one pipeline or as another pipeline's lookup (a hash
/// join's build, which PostgreSQL may never read). Fail-closed: a whole reader on the way (a sort, an aggregate) does not
/// end the walk.
fn chain_halts(gx: &Gx<'_, '_>, r: usize) -> bool {
    let ps = gx.pipelines();
    let mut q = r;
    loop {
        let Some(b) = crate::frame::iface::breaker_of(&ps[q].desc) else { return false };
        let mut readers = (q + 1..ps.len()).filter(|&c| ps[c].desc.consumes.contains(&b.slot));
        let (Some(c), None) = (readers.next(), readers.next()) else { return true };
        if !matches!(ps[c].source, Source::Breaker { slot } if slot == b.slot) || halts(&ps[c].ops) {
            return true;
        }
        q = c;
    }
}

/// D2884: which member of a run of keys equal across subplans the merge emits first shows in `m`'s output. PostgreSQL's
/// MergeAppend drains its heap's top subplan through such a run, the left child before the right on a tie; the pull
/// form replays that heap (merge_pull_heap.rs), the breaker form's run merge emits the run in arm order. Either member
/// is a right answer (a tie choice); `merge_pull` takes the pull form for such a consumer where it reads PostgreSQL's
/// rows (`tie_reads_pg`), so there it shows PostgreSQL's member. Rows tied across subplans agree on every merge key, so
/// a row of merge-key columns whose equal values print alike (types_core eq_alike) shows no member, whatever reads it.
/// Else two consumers show it: the reader's first-of-run op, whose output reads a column outside the merge keys, and a
/// WindowAgg filled from the reader (stacked windows too) with a function that reads its peers' order. Anything else
/// after the first-of-run op but projections and a LIMIT counts as showing it. Not a member: an ORDER BY's ties (a
/// multiset), WITH TIES, a streamed group, a join.
fn tie_shown(gx: &Gx<'_, '_>, m: &Merge<'_>) -> bool {
    use gravity_decompose::catalog::expr_type::{collation_of, result_type, type_of, typmod_of};
    let ps = gx.pipelines();
    let r = &ps[m.reader];
    let Some(at) = r.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. }))) else { return false };
    let after = &r.ops[at + 1..];
    let lead = after.iter().position(|op| !matches!(op, Op::Project(_) | Op::Filter(_))).unwrap_or(after.len());
    let alike = |e: Node<'_>| {
        let ty = type_of(e).unwrap_or_else(|| result_type(e));
        let bytes = gx.facts.coll(collation_of(e)).bytes_eq;
        types_core::eq_alike::alike(ty, typmod_of(e), bytes, false, |t| gravity_decompose::catalog::typefacts::facts_of(gx.facts, t).map(|tf| tf.image.eq_alike()))
    };
    // the consumer's input row: the reader's last projection before it over the merge row, else the merge row (an arm's)
    let keyed = |resno: i16| m.keys.iter().any(|k| k.resno == resno);
    let row: PgVec<'_, bool> = match after[..lead].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) {
        Some(l) => l.iter().map(|n| n.as_target_entry().is_some_and(|te| te.expr.as_const().is_some() || te.expr.as_var().is_some_and(|v| keyed(v.varattno)) && alike(te.expr))).collect_in(gx.mcx),
        None => {
            let Some(cols) = m.arms.last().and_then(|&a| arm_cols(&ps[a])) else { return true };
            cols.iter().enumerate().map(|(c, n)| keyed(c as i16 + 1) && n.as_target_entry().is_some_and(|te| alike(te.expr))).collect_in(gx.mcx)
        }
    };
    let safe = |resno: i16| resno >= 1 && row.get(resno as usize - 1).copied().unwrap_or(false);
    if let Some(Op::FirstOfRun(_)) = after.get(lead) {
        let shown = |l: &NodeList<'_>| l.iter().any(|n| !n.as_target_entry().is_some_and(|te| te.expr.as_const().is_some() || te.expr.as_var().is_some_and(|v| safe(v.varattno))));
        let mut projected = false;
        for op in &after[lead + 1..] {
            match op {
                Op::Project(l) if !projected => {
                    if shown(l) {
                        return true;
                    }
                    projected = true;
                }
                Op::Project(_) => {}
                _ if crate::frame::iface::limit_bounds(op).is_some() => {}
                _ => return true,
            }
        }
        return !projected;
    }
    if lead < after.len() || row.iter().all(|&x| x) {
        return false;
    }
    // a window that reads no column but merge-key ones hands on the same rows whichever tied member came first; else
    // each function must read no peer order (the effects home's readings, effects/visible/merge.rs)
    use crate::effects::visible::merge::{window_peers_free, window_reads_only};
    let (mut q, mut first) = (m.reader, true);
    loop {
        let Some(slot) = crate::frame::iface::breaker_of(&ps[q].desc).map(|b| b.slot) else { return false };
        let Some(next) = (q + 1..ps.len()).find(|&c| ps[c].desc.consumes.contains(&slot)) else { return false };
        if !matches!(ps[next].source, Source::Breaker { slot: s } if s == slot) {
            return false;
        }
        let windows: PgVec<'_, _> = ps[next].ops.iter().filter_map(|op| if let Op::Window(d) = op { Some(d) } else { None }).collect_in(gx.mcx);
        if windows.is_empty() || first && windows.iter().all(|d| window_reads_only(d, &safe)) {
            return false;
        }
        if !windows.iter().all(|d| window_peers_free(d, &alike)) {
            return true;
        }
        (q, first) = (next, false);
    }
}
