//! merge.join as a nest loop's embedded inner (regress psql#479 / #480, D1018). psql's `\dAp` plans a Nested Loop whose
//! inner is a Merge Right Join of an Index Scan on pg_namespace and a Sort of pg_opfamily, with no NestLoopParam. PG 18.6
//! rescans the merge at every outer row (ExecReScanMergeJoin: the state back at INITIALIZE_OUTER, both children rescanned);
//! the inner Sort, randomAccess by the merge's EXEC_FLAG_MARK, rewinds its sorted rows when no parameter changed
//! (ExecReScanSort). Here the inner's run is filled once ahead of the owner (no rerun set holds its fill or slot) and read
//! from its first record at every re-open: the merge's machine words are open words, written again by each re-open
//! (rerun/inner.rs `open`), and its ENDOUTER fill (RIGHT / FULL / RIGHT ANTI) is handed on at the inner's input end
//! (rerun/inner.rs `walk`'s flush). The fill is quiet (markable.rs `inner_may_act`: no raise, no user or volatile call, no
//! user comparator), so where PG fills it (the first fetch that needs the inner, never under an empty outer) shows nothing,
//! and the merge takes no lazy or pulled inner. D3634: or it may only raise (`raise_only`: no user or volatile call, no
//! user comparator, no lock), and the merge runs it whole first under the recheck licence (`ahead`, `Px::run_ahead`): an
//! error re-runs the statement in plan order, where the licence is denied and the merge refuses by `INNER_BEYOND_FORMS`.
//! No Memoize (rerun/inner.rs `op_ok`). A helper file (ROWS = NONE). Owner mergejoin.
use gravity_decompose::effects::{unobservable, Effects, Licence};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;

use crate::frame::px::Px;
use crate::graph::Gx;

/// Whether op `i` of embedded inner `ik` is a merge join the inner serves (the module's rule).
pub(crate) fn ok(gx: &Gx<'_, '_>, ik: usize, i: usize) -> bool {
    let Some(Op::Join(JoinDesc::Merge { slot, .. })) = gx.pipelines()[ik].ops.get(i) else { return false };
    let fillers = crate::ops::mergejoin::lazy_outer::fillers(gx, ik, *slot);
    let held = |x: usize| gx.graph.reruns.iter().any(|r| gravity_decompose::graph::slot_has(&r.slots, *slot) || r.pipelines.iter().any(|&p| p as usize == x));
    !fillers.is_empty() && fillers.iter().all(|&x| !held(x) && (!crate::effects::visible::merge::inner_may_act(gx, x, usize::MAX, None) || crate::effects::visible::merge::raise_only(gx, x)))
}

/// Whether op `j` of pipeline `k` is a merge join this module serves: `k` a nest loop's embedded inner, the merge `ok`.
pub(crate) fn serves(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    let nest = k.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).map(|x| x.0)).is_some_and(|(o, oj)| matches!(ps[o].ops.get(oj), Some(Op::Join(JoinDesc::Nest { .. }))));
    nest && ok(gx, k, j)
}

/// The licence the run's fill, ahead of the owner, runs under where it departs from PG's (its first inner fetch) by nothing
/// that shows: every filler quiet (`inner_may_act`: no raise, no user or volatile call, so `effects::unobservable`), or
/// every one granted the recheck licence (`Px::run_ahead`, recorded on each filler; the licence returned is the last
/// filler's, the same recheck licence as the others'). `None`: the merge refuses. Asked by the merge's lowering (lower.rs
/// `make`). Construct C2: every path that runs the fill ahead holds a `Licence`.
pub(crate) fn ahead(px: &mut Px<'_, '_>, fillers: &[usize]) -> Option<Licence> {
    let gx = px.gx;
    if fillers.iter().all(|&x| !crate::effects::visible::merge::inner_may_act(gx, x, usize::MAX, None)) {
        return unobservable(Effects::NONE);
    }
    let mut last = None;
    for &x in fillers {
        last = Some(px.run_ahead(x, Effects::RAISES)?);
    }
    last
}
