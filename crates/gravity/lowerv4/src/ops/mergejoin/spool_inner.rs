//! merge.join's lazy inner over a spool (W8-D1236-CLASSC, D1236 class C): the inner ends in a Materialize (decompose's
//! keyless spool, PG's Material: both pulled row by row) over a node that acts only through its ONE lazy input, made before
//! it: an INNER / SEMI / RIGHT / RIGHT ANTI merge's (MERGE R3 (ii)) or a D824 probe's, which fires before that node hands a
//! row on, inside its FIRST pull (PG's MJFillOuter hands a LEFT / ANTI row on first: D1236). Its rows are then only
//! filtered, projected or a nest loop's outer (a LIMIT, gate or probe may leave it unpulled: D1236), so the parent's (ii)
//! group reads the spool whole at INITIALIZE_INNER, where a Sort inner acts. Parent INNER / LEFT / SEMI / ANTI over a quiet
//! outer; never over a CTE's store, which fills only at its readers' pulls (D2178). (EXPLAIN ANALYZE of any Merge Join
//! is the row executor's: exec v4/m12.rs HEAP.) A helper file (ROWS = NONE). Owner M11-MERGE. Budget 40 lines.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use mcx::{CollectIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::effects::visible::merge::may_act_but;
use crate::frame::px::Px;

/// Merge `(k, j)` of type `jt` over pipeline `inner` in slot `slot`, not Sort-ended: its (ii) group may read it whole.
pub(crate) fn first_pull(px: &Px<'_, '_>, (k, j): (usize, usize), inner: usize, slot: u8, jt: JoinType) -> bool {
    let (gx, p) = (px.gx, &px.gx.pipelines()[inner]);
    let own: PgVec<'_, u8> = p.desc.consumes.iter().copied().filter(|&s| gx.life_of(inner, s).is_some_and(|l| px.lazy.contains(&(l.0 as usize, inner)))).collect_in(px.mcx());
    let at = p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, .. } | JoinDesc::Probe { slot, .. }) if own == [*slot]));
    let hands = |op: &Op<'_>| !matches!(op, Op::Join(JoinDesc::Merge { jointype, .. }) if !matches!(*jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI));
    let plain = |ops: &[Op<'_>]| ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Nest { .. } | JoinDesc::Spool)));
    matches!(crate::frame::iface::end_breaker(gx, inner), Some(BreakerKind::Materialize))
        && matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
        && at.is_some_and(|a| hands(&p.ops[a]) && plain(&p.ops[a + 1..]))
        && !may_act_but(gx, k, j, Some(slot))
        && !crate::effects::visible::merge::inner_may_act(gx, inner, usize::MAX, own.first().copied())
        && gx.run_arms(inner).is_empty() // D2026: one pipeline fills the spool
        && !gx.cte_write(inner) // D2178: a CTE's store fills only at its readers' pulls (m8_cte_pull), never in a build wait
}
