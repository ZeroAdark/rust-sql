//! merge.join's fed-first inner (W8-D1236-CLASSD, subselect#314's form): an INNER / LEFT / SEMI / ANTI merge over an
//! acting outer PG surely runs (lazy_outer.rs `acting_outer`), whose Sort-ended inner acts only through the outer's feeders,
//! each read whole (a CTE both sides read). PG's INITIALIZE_OUTER reads the outer's Sort whole, its CTE Scan computing the
//! CTE whole before INITIALIZE_INNER, whose own pipelines only read the store: they act nothing, so v4 runs them unwaited
//! in program order (an empty outer included) where D824's lazy group does not apply (`leaves`: the CTE's last reader is
//! the outer). The inner pulls the CTE first, so its acts may run before the outer's own ops on a row: where that order
//! shows (D2275, `interleaves`), the form leaves this road. EXPLAIN ANALYZE of a Merge Join is the row executor's. The
//! admission list is empty (D2248 left with its root fix, expr.saop's Desc fingerprint, saop_host.rs). D3374: the
//! merge may be a recursive term's body PG runs whole at every generation (`term`), and an outer feeder may be the CTE's
//! own recursive union, which the outer's whole read runs to its end (`union_whole`). Helper (ROWS = NONE). Owner
//! M11-MERGE.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::{Op, PipelineNode, Source};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::Node;

use crate::ops::mergejoin::strategies::markable::pipe_quiet;
use crate::graph::Gx;

/// Pipeline `p` reads its whole input into its end (lazy_outer.rs `leaves`): a scan or its one breaker slot, filtered,
/// projected or sorted, into a full Sort's run or a Materialize store (a CTE's).
pub(crate) fn whole(p: &PipelineNode<'_>) -> bool {
    let src = matches!(p.source, Source::Range { .. }) || matches!(p.source, Source::Breaker { slot } if p.desc.consumes.iter().all(|&s| s == slot));
    let store = crate::frame::iface::breaker_of(&p.desc).is_some_and(|e| e.kind == BreakerKind::Materialize) && !p.ops.iter().any(|op| matches!(op, Op::Sort(_)));
    src && p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Sort(_))) && (crate::ops::mergejoin::lazy_outer::full(p) || store)
}

/// D3374: pipeline `p` is a recursive union's driver (its `Iter` op first, its source the seed's table, the term's next
/// table the other slot it reads) into the CTE's store through filters and projections, every reader of the union reading
/// it to its end (`IterDesc::read_whole`, decompose ops/setop.rs `readers`): the outer's first fetch, reading the CTE whole,
/// runs it to its end (PG's RecursiveUnion under the outer Sort's CTE Scan), its seed and term pipelines `whole` too.
fn union_whole(p: &PipelineNode<'_>) -> bool {
    let store = crate::frame::iface::breaker_of(&p.desc).is_some_and(|e| e.kind == BreakerKind::Materialize);
    let plain = |ops: &[Op<'_>]| ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    store && matches!(p.ops.split_first(), Some((Op::SetOp(SetDesc::Iter(d)), rest)) if d.read_whole && plain(rest)
        && matches!(p.source, Source::Breaker { slot } if slot == d.work) && p.desc.consumes.iter().all(|&s| s == d.work || s == d.next))
}

/// D3374 (rerun_nested_recursive N13): merge `(k, j)` is a recursive term's body that PG runs to its end at every
/// generation the lowering runs: pipeline `k` ends in a recursive union's next table (`IterDesc::next`, `k` its `body`)
/// through filters and projections only, and every reader of the union reads it to its end (`IterDesc::read_whole`), so
/// PG's RecursiveUnion pulls the term until it returns no row in each generation, as the lowering runs a generation whole.
/// With the outer `lazy_outer::outer_form` takes, the fed-first road holds there as where `lazy_outer::reached` does.
pub(crate) fn term(gx: &Gx<'_, '_>, (k, j): (usize, usize), before: &[usize], all: &[usize]) -> bool {
    let ps = gx.pipelines();
    let Some(e) = crate::frame::iface::breaker_of(&ps[k].desc).filter(|e| e.kind == BreakerKind::Materialize) else { return false };
    let plain = ps[k].ops.get(j + 1..).unwrap_or(&[]).iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    let body = (0..ps.len()).any(|c| ps[c].ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(d)) if d.body as usize == k && d.next == e.slot && d.read_whole)));
    let ok = plain && body && crate::ops::mergejoin::lazy_outer::outer_form(gx, k, j, before, all);
    if ok {
        fires::fire("merge.inner.fed_term"); // shapecheck fire signal (D3374: the fed-first road in a recursive term)
    }
    ok
}

/// Merge inner `inner` acts only through `before` (the outer's feeders), each read whole: every pipeline its slots reach
/// outside `before` is quiet (markable.rs `may_act`'s walk, `before` taken as run). `Some`: those
/// quiet pipelines, the inner's own, which run unwaited (construct C2: they hold the no-effects licence, lower.rs `make`).
pub(crate) fn fed_first<'g>(gx: &Gx<'g, '_>, inner: usize, before: &[usize]) -> Option<PgVec<'g, usize>> {
    let (ps, mut seen): (_, PgVec<'g, usize>) = (gx.pipelines(), PgVec::new_in(gx.mcx));
    let mut todo: PgVec<'_, usize> = gx.run_arms(inner).into_iter().chain([inner]).collect_in(gx.mcx); // (D2026: every arm)
    while let Some(k) = todo.pop() {
        if seen.contains(&k) || before.contains(&k) && k != inner {
            continue;
        }
        seen.push_in(k);
        let mut feeds = PgVec::new_in(gx.mcx);
        if !pipe_quiet(gx, &ps[k], usize::MAX, &mut feeds) || crate::effects::visible::merge::user_sort(gx, &ps[k]) {
            return None;
        }
        todo.extend_in(feeds.into_iter().flat_map(|s| crate::ops::mergejoin::lazy_outer::fillers(gx, k, s)).chain(crate::ops::mergejoin::lazy_outer::settled(gx, k)));
    }
    let fed = before.iter().all(|&x| whole(&ps[x]) || union_whole(&ps[x])) && !interleaves(gx, before);
    fed.then_some(seen)
}

/// D2275: PG's INITIALIZE_OUTER computes the CTE row by row inside the outer Sort's read, running the outer's own ops on
/// each row before the CTE's next; the fed-first road has the inner pull the CTE whole first. Where one of the outer's
/// feeders (`before`: the CTE, the outer's scan of it) acts (a VOLATILE or user call, effects' `user_fn` / `volatile`) and
/// another may raise or act (`pipe_quiet`), that order shows (a nextval count, a currval read, NOTICE order): not
/// fed-first, D1236's paths decide. Which of two feeders' errors raises first is free (either answer is PG's), so feeders
/// that only may raise stay.
fn interleaves(gx: &Gx<'_, '_>, before: &[usize]) -> bool {
    let ps = gx.pipelines();
    let acts_n = |n: Node<'_>| crate::effects::user_or_volatile(gx, n);
    let acts = |k: usize| ps[k].ops.iter().any(|op| match op {
        Op::Filter(n) | Op::Gate(n) => n.as_list().map_or_else(|| acts_n(*n), |l| l.iter().any(acts_n)),
        Op::Project(l) => l.iter().any(acts_n),
        _ => false,
    });
    let quiet = |k: usize| pipe_quiet(gx, &ps[k], usize::MAX, &mut PgVec::new_in(gx.mcx));
    before.iter().any(|&a| acts(a) && before.iter().any(|&b| b != a && !quiet(b)))
}
