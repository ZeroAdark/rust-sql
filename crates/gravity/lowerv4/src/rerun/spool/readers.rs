//! nest.spool's reader walk (M8-NEST D1919, R1 of the LTC-NEST-DRAIN design; split from spool_form.rs, a lib.rs module):
//! the one pipeline that reads a plan Material's life. PostgreSQL's Material has one parent. Decompose keeps a nest's
//! inner inside the op that runs it, and the pipeline that embeds an inner also lists every life the inner reads
//! (breakers.rs `embed`), so under two nest levels the inner between lists the life as its reader does. The reader is the
//! pipeline that reads the life through its own source or ops; a pipeline (graph pipeline or inner) is an owner when every
//! read of the life in it goes through an inner it embeds, never through its own source or ops (review 4). The reader must
//! be a nest inner over the Material's breaker, linked to its graph pipeline by nests or (D1109) nested SubPlans only: a
//! nested SubPlan's inner re-opens per call as a nest's per outer row (PG's ExecScanSubPlan rescans the plan, and its
//! Material rewinds the rows it holds: ExecReScanMaterial with no changed parameter); any other embedding op is R2's. The
//! rerun guard (decision 2): a fill pipeline any rerun set re-runs refuses by D1107's name, as the set would append again
//! under the spool's count where PostgreSQL refills on a changed parameter (ExecReScanMaterial). `walk_bound`: D1630's
//! class, which a statement B-R1 newly serves refuses by name (spool_form.rs, nest/spool_site.rs).
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::scan::{BitmapPlan, TidForm};
use gravity_decompose::graph::{IndexKey, KeyForm};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, Source};
use gravity_program::LifeId;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::Node;

use crate::graph::Gx;
use crate::rerun::spool::form::{REFILL_CHILD, MANY_READERS};

/// One pipeline of the view (graph pipelines, then the inners in view order) as the walk sees it.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    /// An inner's container, the pipeline whose op embeds it (`None`: a graph pipeline).
    up: Option<usize>,
    /// The op embedding it re-opens it per outer row or call: a nest, or (D1109) a nested SubPlan.
    nest: bool,
    /// Its descriptor lists the life: its own read or an inner's it embeds.
    reads: bool,
    /// It reads the life through its source (`src`) or one of its own ops (`own`, the source's read included).
    src: bool,
    own: bool,
}

/// The breaker slots op `op` reads itself (an embedded inner's are the inner's).
fn slots(op: &Op<'_>) -> [Option<u8>; 2] {
    match op {
        Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) => [Some(*slot), None],
        Op::Join(JoinDesc::Drain { matches, .. }) => [Some(*matches), None],
        Op::SubPlan(SubPlanDesc { form: SubForm::Hashed { slot }, .. }) => [Some(*slot), None],
        Op::Append(AppendDesc::Feed { slot, .. } | AppendDesc::Union { slot, .. } | AppendDesc::Merge { slot, .. }) => [Some(*slot), None],
        Op::SetOp(SetDesc::Set { left, right, .. }) => [Some(*left), Some(*right)],
        _ => [None, None],
    }
}

/// Pipeline `k`'s plan Material's one reader (its view index), else D1109 by name.
pub(crate) fn reader(gx: &Gx<'_, '_>, k: usize) -> Result<usize, &'static str> {
    let ps = gx.pipelines();
    let life = Some(LifeId(k as u16));
    let (n, inners) = (ps.len(), ps.inners());
    let seen: PgVec<'_, Seen> = (0..n + inners.len())
        .map(|i| {
            let q = &ps[i];
            let from = |s: u8| q.desc.consumes.iter().position(|&x| x == s).and_then(|j| q.desc.consumes_from.get(j).copied());
            let of = |s: u8| if i < n { gx.life_of(i, s) } else { from(s) };
            let src = matches!(q.source, Source::Breaker { slot } if of(slot) == life);
            let own = src || q.ops.iter().any(|op| slots(op).into_iter().flatten().any(|s| of(s) == life));
            let (up, nest) = match i.checked_sub(n).map(|x| inners[x].0) {
                // D1109 (R2's SubPlan link): a nested SubPlan's inner re-opens per call as a nest's per outer row
                Some((o, j)) => (Some(o), matches!(ps[o].ops.get(j), Some(Op::Join(JoinDesc::Nest { .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { .. }, .. })))),
                None => (None, false),
            };
            Seen { up, nest, reads: q.desc.consumes.iter().any(|&s| of(s) == life), src, own }
        })
        .collect_in(gx.mcx);
    one_reader(&seen[..], k).filter(|&r| seen[r].src).ok_or(MANY_READERS)
}

/// `reader`, or (rowsecurity#343) the first of `copies`: `(reader, false)` for the one reader, `(first copy, true)` for
/// the nest copies of a streamed write's arms, else D1109 by name.
pub(crate) fn reader_or_copies(gx: &Gx<'_, '_>, k: usize) -> Result<(usize, bool), &'static str> {
    match reader(gx, k) {
        Ok(r) => Ok((r, false)),
        Err(e) => copies(gx, k).and_then(|c| c.first().copied()).map(|r| (r, true)).ok_or(e),
    }
}

/// rowsecurity#343 (decompose arm_nests.rs): pipeline `k`'s plan Material read by the copies of one nest in the arms of a
/// streamed union, PostgreSQL's one Materialize under one Nested Loop: two or more inners (their view indices, in order),
/// each embedded one level down by a Nest op of a graph pipeline and reading the life through its source, all of one plan
/// node, their containers the serial arms of one streamed union (`Gx::stream_arms`), and no other pipeline reading the
/// life (a container lists it through the inner it embeds). D4569: a SELECT's copies (`union_nest`) may sit in arms that
/// run wide: their fill is the calm eager one, run once in pipeline order ahead of every arm (lazy.rs keeps it there).
pub(crate) fn copies<'g>(gx: &Gx<'g, '_>, k: usize) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let life = Some(LifeId(k as u16));
    let (n, inners) = (ps.len(), ps.inners());
    let mut out = PgVec::new_in(gx.mcx);
    let mut select = true;
    for (x, ((o, j), _)) in inners.iter().enumerate() {
        let q = &ps[n + x];
        let from = |s: u8| q.desc.consumes.iter().position(|&y| y == s).and_then(|i| q.desc.consumes_from.get(i).copied());
        if !q.desc.consumes.iter().any(|&s| from(s) == life) {
            continue;
        }
        let src = matches!(q.source, Source::Breaker { slot } if from(slot) == life);
        if !src || *o >= n || !matches!(ps[*o].ops.get(*j), Some(Op::Join(JoinDesc::Nest { .. }))) {
            return None;
        }
        select &= union_nest(gx, *o, *j);
        out.push_in(n + x);
    }
    let up = |i: usize| inners[i - n].0 .0;
    let first = *out.first()?;
    let arms = gx.stream_arms(up(first));
    let same = out.iter().all(|&i| ps[i].node_id == ps[first].node_id && (up(i) == up(first) || arms.contains(&up(i))) && (select || !ps[up(i)].desc.is_eligible()));
    let mut ups: PgVec<'_, usize> = out.iter().map(|&i| up(i)).collect_in(gx.mcx);
    ups.dedup();
    let only = (0..n).all(|c| c == k || ups.contains(&c) || !ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == life));
    (out.len() > 1 && ups.len() == out.len() && same && only).then_some(out)
}

/// D4569 (decompose arm_nests.rs `select`): the Nest at op `j` of graph pipeline `o` is a SELECT's union nest copy, one
/// decompose found quiet (`PipelineGraph::union_nests`: no parameters, a calm inner, a quiet join qual), in an arm that
/// writes and locks nothing.
pub(crate) fn union_nest(gx: &Gx<'_, '_>, o: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    if o >= ps.len() {
        return false;
    }
    let Some(Op::Join(JoinDesc::Nest { facts, .. })) = ps[o].ops.get(j) else { return false };
    gx.graph.union_nests.contains(&facts.node) && !ps[o].ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_)))
}

/// R1 over `seen` (`k` the fill pipeline): the one pipeline that reads the life and is no owner, when it is an inner
/// linked to its graph pipeline by nests only.
fn one_reader(seen: &[Seen], k: usize) -> Option<usize> {
    let under = |j: usize, i: usize| core::iter::successors(seen[j].up, |&u| seen[u].up).any(|u| u == i);
    let owner = |i: usize| !seen[i].own && (0..seen.len()).any(|j| j != i && seen[j].reads && under(j, i));
    let mut readers = (0..seen.len()).filter(|&i| i != k && seen[i].reads && !owner(i));
    let r = readers.next()?;
    let nests = core::iter::successors(Some(r), |&x| seen[x].up).all(|x| seen[x].up.is_none() || seen[x].nest);
    (readers.next().is_none() && seen[r].up.is_some() && nests).then_some(r)
}

/// D1630's class where B-R1 serves (spool_form.rs `D1630`): some inner of the graph walks by a word bound once at bind (no
/// constant, parameter or executor parameter's; nest/inner_once.rs's test in D1630's row): an index or bitmap key, a TID
/// value, a TABLESAMPLE argument. PostgreSQL evaluates it at the inner's scan only, never under an empty outer.
pub(crate) fn walk_bound(gx: &Gx<'_, '_>) -> bool {
    let ps = gx.pipelines();
    let tid = |q: Node<'_>| q.as_op_expr().map(|o| &o.args).or_else(|| q.as_scalar_array_op_expr().map(|a| &a.args)).is_some_and(|a| a.iter().any(|v| v.as_var().is_none() && bound(v)));
    ps.inners().iter().any(|(_, q)| match &q.source {
        Source::Index { keys, .. } => keys_bound(keys),
        Source::Bitmap { plan, .. } => bitmap_bound(plan),
        Source::Tid { form, quals, .. } => *form != TidForm::Current && quals.iter().any(tid),
        Source::Sample { sample, .. } => sample.args.iter().chain(sample.repeatable).any(bound),
        _ => false,
    })
}

fn bound(v: Node<'_>) -> bool {
    v.as_const().is_none() && v.as_param().is_none() && !gravity_lower::rerun::words::has_exec(v)
}

fn keys_bound(keys: &[IndexKey<'_>]) -> bool {
    keys.iter().any(|k| match k.form {
        KeyForm::Null | KeyForm::NotNull => false,
        KeyForm::Row => k.value.as_row_compare_expr().is_some_and(|rc| rc.rargs.iter().any(bound)),
        _ => bound(k.value),
    })
}

fn bitmap_bound(p: &BitmapPlan<'_>) -> bool {
    crate::stack_check();
    let (BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. }) = p else { return matches!(p, BitmapPlan::Index { keys, .. } if keys_bound(keys)) };
    plans.iter().any(bitmap_bound)
}

/// Decision 2's guard: `Err(D1107)` when a rerun set (any: a nest's or a SubPlan's) re-runs the fill pipeline `k`.
/// D3217: but not a set kept whole (rerun/sets.rs `kept_whole`: the fill of a hashed Agg a nest inner reads, whose table
/// ExecReScanAgg keeps over a rescan that changes no param), the fill's one set: it runs once, as PostgreSQL runs the
/// Agg's input, the Material's fill and its readers once.
pub(crate) fn rerun_free(gx: &Gx<'_, '_>, k: usize) -> Result<(), &'static str> {
    let mut sets = gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(k as u16)));
    match (sets.next(), sets.next()) {
        (None, _) => Ok(()),
        (Some(r), None) if crate::rerun::sets::kept_whole(gx, r) => Ok(()),
        _ => Err(REFILL_CHILD),
    }
}

#[cfg(test)]
mod tests {
    use super::{one_reader, Seen};

    /// A graph pipeline (`up` None) or an inner under `up`, linked by a nest (or a nested SubPlan) unless `sub`.
    fn p(up: Option<usize>, sub: bool, reads: bool, src: bool, own: bool) -> Seen {
        Seen { up, nest: up.is_some() && !sub, reads, src, own: own || src }
    }

    /// The fill pipeline 0 (no reader of its own life) and its owner graph pipeline 1.
    fn fill_and_owner() -> Vec<Seen> {
        vec![p(None, false, false, false, false), p(None, false, true, false, false)] // alloc: Program
    }

    #[test]
    fn one_level_the_inner_reads() {
        let mut s = fill_and_owner();
        s.push(p(Some(1), false, true, true, false));
        assert_eq!(one_reader(&s, 0), Some(2));
    }

    /// D1919: the inner between (it only embeds the reader) is an owner, at 2 and 3 levels.
    #[test]
    fn the_innermost_inner_reads_at_two_and_three_levels() {
        let mut s = fill_and_owner();
        s.push(p(Some(1), false, true, false, false));
        s.push(p(Some(2), false, true, true, false));
        assert_eq!(one_reader(&s, 0), Some(3));
        s.push(p(Some(3), false, false, false, false)); // an inner beside the reader, over another source
        assert_eq!(one_reader(&s, 0), Some(3));
        let mut t = fill_and_owner();
        t.push(p(Some(1), false, true, false, false));
        t.push(p(Some(2), false, true, false, false));
        t.push(p(Some(3), false, true, true, false));
        assert_eq!(one_reader(&t, 0), Some(4));
    }

    /// Review 4's condition: two inners that each read the Material as their source (one embedding the other) are two
    /// readers, and so are an inner reading it through an op (a probe) beside the inner it embeds.
    #[test]
    fn an_inner_reading_the_life_itself_is_no_owner() {
        let mut s = fill_and_owner();
        s.push(p(Some(1), false, true, true, false));
        s.push(p(Some(2), false, true, true, false));
        assert_eq!(one_reader(&s, 0), None);
        let mut t = fill_and_owner();
        t.push(p(Some(1), false, true, false, true));
        t.push(p(Some(2), false, true, true, false));
        assert_eq!(one_reader(&t, 0), None);
    }

    /// A link that does not re-open per row (`sub`: an embedding op other than a nest or a nested SubPlan; R2's) anywhere
    /// up the chain, a graph pipeline reading the Material itself, a second reader in another nest of the owner, and no
    /// reader: none.
    #[test]
    fn other_readers_refuse() {
        let mut s = fill_and_owner();
        s.push(p(Some(1), true, true, false, false));
        s.push(p(Some(2), false, true, true, false));
        assert_eq!(one_reader(&s, 0), None);
        let mut g = vec![p(None, false, false, false, false), p(None, false, true, true, false)]; // alloc: Program
        assert_eq!(one_reader(&g, 0), None);
        g[1] = p(None, false, true, false, false);
        g.push(p(Some(1), false, true, true, false));
        g.push(p(Some(1), false, true, true, false));
        assert_eq!(one_reader(&g, 0), None);
        assert_eq!(one_reader(&[p(None, false, false, false, false)], 0), None);
    }
}
