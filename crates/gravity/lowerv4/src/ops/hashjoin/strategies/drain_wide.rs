//! D4569 (road `drain.spool.wide`): join.drain's spooled form (row A1b, drain/spool.rs) with its probe run at width. The
//! spooled form's probe pipeline `k` writes its joined rows into a spool on the leader (`DrainFill`, one filler), then the
//! drain pipeline `d` reads them and the drained build records through its own ops (`DrainReader`). Where nothing shows
//! which worker joined which rows, the probe fills an OWNED spool instead (D3040: each worker appends to its own slot,
//! exec seals the slots into one region after the probe, rerun/spool/rec.rs), and the reader puts each drained record
//! into a one-record spool of its own (`Rec::with_put`), the sealed region taking no record. A union arm that is a
//! returns table LEFT JOIN its sales table under a union nest D4569's append.nest copies (its tail a nest the fused form
//! cannot run) then probes the sales table on every worker. The joined rows reach the drain in the slots' order, not
//! PostgreSQL's probe order, so the road asks for a drain whose end does not show the order (A2END's order-free end),
//! with no LIMIT in its tail, in a statement that neither writes nor locks, rechecks nothing and is read whole:
//! - the probe a graph pipeline decompose made eligible for width, run once (no rerun set holds it or the drain), its
//!   spool unstreamed and its own (not a streamed probe's, not a union of drained arms' one spool, not a nest inner's);
//! - the probe quiet (N1's (a), `Gx::spool_quiet`): its scan, its ops and its probe raise nothing and call nothing visible;
//! - a RIGHT or FULL drain (a RIGHT SEMI probe may hand a build record's first match into the spool: two workers could
//!   both hand it);
//! - the drain's end not a spool (the put spool is declared for the drain's pipeline).
//!
//! Off (`SET pgrust.gravity_road_off = 'drain.spool.wide'`): the probe fills the spool on the leader, as before.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use types_nodes::jointype::JoinType;

use crate::graph::{EndKind, Gx};
use crate::ops::Table;

crate::strategies! {
    /// A spooled drain's quiet probe fills an owned spool at width; the drain reads it on the leader.
    DRAIN_SPOOL_WIDE = 146, "drain.spool.wide", Drain;
}

pub(crate) const ROWS: Table = Table { strategies: &[(DRAIN_SPOOL_WIDE, &())], ..crate::ops::NONE };

/// Probe `k`'s spooled drain `d` (its Drain op `jd`) takes this road (the file's head).
pub(crate) fn takes(gx: &Gx<'_, '_>, k: usize, d: usize, jd: usize) -> bool {
    let ps = gx.pipelines();
    if !crate::face::end::Parallel::DrainWide.on() || k >= ps.len() || d >= ps.len() || k >= d {
        return false;
    }
    let g = gx.graph;
    let acts = ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    let rerun = |q: usize| g.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    if acts || g.recheck || g.partial || rerun(k) || rerun(d) || !g.record_rows.is_empty() {
        return false;
    }
    // a RIGHT or FULL drain: a RIGHT SEMI one's probe may hand a build record's first match into the spool (spool.rs
    // `semi_hands`), which two workers could both hand; a RIGHT ANTI one hands nothing
    let jt = matches!(ps[d].ops.get(jd), Some(Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL, .. })));
    jt && matches!(ps[k].desc.parallel, gravity_program::Parallel::Eligible)
        && gx.drained_arms(k).is_none()
        && !gx.drain_streams(k, d, jd)
        && gx.spool_quiet(k)
        && gx.end_kind(d) != EndKind::Breaker(BreakerKind::Materialize)
        && !crate::frame::iface::holds_limit(&ps[d].ops[jd + 1..])
        && crate::effects::visible::drain::order_free_in(gx, d, false)
}
