//! The sort family's visible decisions in the effects home: whether a Sort's raise at its first call can be served
//! (`raising`, read by ops/sort/sort_raise.rs), and whether a group's finals must run once (`finals_once`, read by
//! ops/sort/shapes.rs and the props).
use gravity_decompose::effects::sort_raises;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::ops::sort;
use gravity_decompose::ops::shared::sort::SortDesc;
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::{Op, PipelineNode, Source};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::graph::Gx;

/// D1292: whether pipeline `k`'s Sort `d` raises (decompose `effects::sort_raises`: a key over a collatable type with no
/// collation; PG's first ExecSort call begins the tuplesort, whose varlena.c `varstr_sortsupport` raises 42P22); a Shape
/// refusal under D1292's name when it does outside the served scope: every graph pipeline but the root a producer into
/// `k`'s breaker over a source of its own (no breaker's rows: nothing runs before the first producer's open), or `k`
/// itself over one breaker whose feeders all have sources of their own (a top over an Append: the feeders' opens raise
/// first, ops/sort/sort_raise.rs `wrap_feeder`); the root that breaker's reader with only projections, filters and a
/// Limit of a constant count past 0 (no gate or other Limit that could leave the Sort unstarted), or a WindowAgg's buffer
/// between (`fill`: a pipeline that reads the Sort with such ops and fills a Materialize, which the root reads with a
/// window op of no offset frame among those: PG's WindowAgg pulls the Sort at its first call), and no init plan.
pub(crate) fn raising(gx: &Gx<'_, '_>, k: usize, d: &SortDesc<'_>) -> bool {
    if !sort_raises(gx.facts, d) {
        return false;
    }
    if !first_act(gx, k) {
        refuse(gx.pipelines()[k].node_id)
    }
    true
}

/// `raising`'s served scope, shared with W8-EQUIVCLASS-SORTOP's root form (ops/sort/sort_noproc.rs): pipeline `k`'s
/// Sort begins as the program's first act (a WindowAgg's buffer between included), so a raise at its producers' open is
/// PG's first ExecSort call.
pub(crate) fn first_act(gx: &Gx<'_, '_>, k: usize) -> bool {
    let g = gx.graph;
    let Some(br) = gx.pipelines()[k].desc.breaker() else { return false };
    let root = g.root as usize;
    let own = |p: &PipelineNode<'_>| !matches!(p.source, Source::Breaker { .. });
    let fed = match gx.pipelines()[k].source { Source::Breaker { slot } => Some(slot), _ => None };
    let producer = |p: &PipelineNode<'_>| {
        own(p) && (p.desc.breaker() == Some(br) || p.desc.breaker().is_some_and(|x| Some(x.slot) == fed))
    };
    let reads = |r: &PipelineNode<'_>, slot: u8| matches!(r.source, Source::Breaker { slot: s } if s == slot) && r.ops.iter().all(sort::starts_raise);
    let fill = g.pipelines.iter().position(|p| reads(p, br.slot) && p.desc.breaker().is_some_and(|x| x.kind == BreakerKind::Materialize));
    let reader = g.pipelines.get(root).is_some_and(|r| {
        reads(r, br.slot) || fill.and_then(|f| g.pipelines[f].desc.breaker()).is_some_and(|m| reads(r, m.slot) && r.ops.iter().any(|o| matches!(o, Op::Window(_))))
    });
    // v0.4: or a chain of pullers up to the root: each reads the one before's breaker as its source alone, by ops that
    // pull at their first call, and ends in a breaker whose PG node reads its whole input at its own first call (an
    // aggregate, a hashed DISTINCT, a Sort), so the root's first call reaches the Sort first (`count(*)` over an ordered
    // OFFSET 0 subquery)
    let chain = || -> Option<PgVec<'_, usize>> {
        let mut seen = PgVec::new_in(gx.mcx);
        let mut slot = br.slot;
        loop {
            let mut r = g.pipelines.iter().enumerate().filter(|(_, p)| matches!(p.source, Source::Breaker { slot: s } if s == slot));
            let (Some((q, p)), None) = (r.next(), r.next()) else { return None };
            if !p.ops.iter().all(sort::starts_raise) || p.union.is_some() || seen.len() > g.pipelines.len() {
                return None;
            }
            if q == root {
                return Some(seen);
            }
            let x = p.desc.breaker().filter(|x| matches!(x.kind, BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct | BreakerKind::Sort))?;
            seen.push_in(q);
            slot = x.slot;
        }
    };
    let chained = if reader { None } else { chain() };
    // v0.4: or a quiet sorted feed of its own source (a merge's other side: its scan, filters and projections raise and
    // call nothing, then one full Sort, whose own raise, if any, is its reader's), which PG runs only after the first
    // call that raised: running it first shows nothing
    let quiet_feed = |p: &PipelineNode<'_>| {
        own(p) && matches!(p.ops.last(), Some(Op::Sort(d)) if matches!(d.form, gravity_decompose::ops::shared::sort::SortForm::Runs)) && crate::effects::span_quiet(gx, p, 0, p.ops.len() - 1, false)
    };
    g.init_plans.is_empty()
        && (reader || chained.is_some())
        && g.pipelines.iter().enumerate().all(|(i, p)| i == root || i == k || Some(i) == fill || chained.as_ref().is_some_and(|c| c.contains(&i)) || producer(p) || quiet_feed(p))
}

fn refuse(node: i32) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(gravity_decompose::ops::shared::sort::INDETERMINATE_COLL) })
}

/// D1403: group `g` holds a user-defined aggregate whose finalfn acts (`effects::of_final`: VOLATILE, or a catalog that
/// cannot say): PostgreSQL finalizes each group once, so no sort orders its records (form G, `runs.topk_groups`: the order
/// pass, the emit and the drain each build the group's row); its top or sort orders the emitted rows by value
/// (`runs.topk_rows`, `runs.sort`). A STABLE or IMMUTABLE finalfn does not act, so calling it again per group is unseen.
pub(crate) fn finals_once(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    g.aggs.iter().any(|d| d.class == gravity_decompose::ops::shared::agg_class::Class::User && gravity_decompose::effects::of_final(gx.facts, d.fnoid).acts)
}

/// Pipeline `k` is the feeding sort: a heap scan's rows, through filters and projections only, into one full sort
/// (`SortForm::Runs`, no presorted prefix, no form-G group sort) whose keys order by their type's default btree family (no
/// CompareFn or no-order-proc key), outside every union arm and rerun set; and its run's one reader is `user_fed`'s
/// consumer.
pub(crate) fn feeds_user_sort(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some(br) = p.desc.produces() else { return false };
    let source = |j: usize| matches!(gx.pipelines()[j].source, Source::Breaker { slot } if slot == br);
    let reads = |j: usize| (gx.pipelines()[j].desc.consumes.contains(&br) || source(j)) && gx.life_of(j, br).is_some_and(|l| l.0 as usize == k);
    let readers: PgVec<'_, usize> = (0..gx.pipelines().len()).filter(|&j| j != k && reads(j)).collect_in(gx.mcx);
    matches!(readers[..], [r] if source(r) && user_fed(gx, r)) && crate::ops::sort::exact_feed::plain_sort(gx, k)
}

/// Pipeline `r` is the consumer: a breaker's rows, through filters and projections only, into a full sort one of whose keys
/// orders by a comparator in SQL or a procedural language (`rep::keylane::c_level_cmp` false, the USER_ARRIVAL case).
pub(crate) fn user_fed(gx: &Gx<'_, '_>, r: usize) -> bool {
    let p = &gx.pipelines()[r];
    let Some(s) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return false };
    let Op::Sort(d) = &p.ops[s] else { return false };
    matches!(p.source, Source::Breaker { .. })
        && p.ops[..s].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
        && d.keys.iter().any(|key| key.compare_fn() && !crate::rep::keylane::c_level_cmp(gx, key.opno))
}
