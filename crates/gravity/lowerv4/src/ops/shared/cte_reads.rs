//! What a CTE reader's pipeline reads, and a CTE pull's window: the pipelines whose slots a pipeline reads (its consumed
//! lives and its source's, each with every arm of the append.arms run it ends), read by the CteScan pull, the streamed
//! drain's pull and the frame's pulled-set check.
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Op, Source};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::iface::breaker_of;
use crate::graph::Gx;

/// A quiet CTE's rows per pull (and a reader's window).
pub(crate) const WINDOW: i64 = 4096;

/// The pipelines whose slots pipeline `p` reads: its consumed lives and its source's, each life with every arm of the
/// append.arms run it ends (`arms`).
pub(crate) fn reads<'g>(gx: &Gx<'g, '_>, p: usize) -> PgVec<'g, usize> {
    let pl = &gx.pipelines()[p];
    let src = if let Source::Breaker { slot } = pl.source { Some(slot) } else { None };
    let lives = pl.desc.consumes.iter().copied().chain(src).filter_map(|s| gx.life_of(p, s).map(|l| (l.0 as usize, s)));
    lives.filter(|&(l, _)| l < gx.pipelines().len()).flat_map(|(l, s)| arms(gx, l, s)).collect_in(gx.mcx)
}

/// Producer `l` of `slot` and, when it is an arm of an append.arms run, the run's earlier arms: the nearest earlier producers
/// of the slot under the same union's outermost marker, down to arm 0 (D1387: an arm may span several). The run time pulls
/// every arm (program `arms_of`); `Gx::life_of` names only the last, so an earlier arm's once-run producer escaped D2081 /
/// D2059 (review r3 S1: a SIGSEGV, 'read outside its life', 2,184 rows for PG's 1,999).
fn arms<'g>(gx: &Gx<'g, '_>, l: usize, slot: u8) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let arm = |q: usize| {
        let (j, i) = ps[q].ops.iter().enumerate().rev().find_map(|(j, o)| if let Op::Append(AppendDesc::Arm { i, .. }) = o { Some((j, *i)) } else { None })?;
        Some((ps[q].node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(ps[q].node_id, |n| n.node_id), i))
    };
    let (mut out, mut at) = (mcx::pg_vec![in gx.mcx; l], arm(l));
    while let Some((node, i)) = at {
        let q = (0..out[out.len() - 1]).rev().find(|&q| matches!(breaker_of(&ps[q].desc), Some(b) if b.slot == slot));
        at = q.and_then(arm).filter(|&(n, j)| n == node && j <= i);
        out.extend_in(q.filter(|_| at.is_some()));
    }
    out
}
