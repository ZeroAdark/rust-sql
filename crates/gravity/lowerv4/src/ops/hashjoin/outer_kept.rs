//! PS1-16 (lanes/dict/DESIGN-PS1-16-17.md §3): whether a hash probe hands its outer row's cells on as they are, so an
//! outer column past it is the probe pipeline's scan lane as read before the probe (table/emit_group.rs `source_var`).
//! The joined row is the probe row's cells, then each payload column's record cell (walk.rs `joined`): an INNER, LEFT or
//! SEMI probe hands an outer row on once per match (a SEMI once, a LEFT miss once with its outer cells as read).
//! It declines (the column's key keeps its value, as before):
//! - another join type, a catalog probe, or a pipeline that drains a build (its end the drain's spool, or a drain run of
//!   arms): a drain's unmatched build rows reach the same group from the drain pipeline, which has no outer site;
//! - a probe join.buildside may swap (build_side.rs `swaps`, the static rule; a stale "no" there is only slower): the
//!   probe row is then the other side's, the outer column a record payload;
//! - a probe whose pipeline's end may take the spill's pair pipeline (spill/build.rs `end_takes_pair`): `P'` re-hands the
//!   kept rows into that end with a code cell read back as its rendered copy (pair.rs, `kept_put`). Every build the probe
//!   reads is then `Spill::None`: `build_bits` takes a probe only through `alone`, which asks that same `end_takes_pair`
//!   of the probe's pipeline (of every arm of its run, D2084). D3057 S1: a keyed group's end may take the pair too, and
//!   then this declines, so such a key keeps its value in `q` and `P'` alike.
//!
//! Owner hashjoin; built by topn-g4 for dict's PS1-16 (the design's predicate, called from `source_var`).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use types_nodes::jointype::JoinType;

use crate::face::kinds::KindId;
use crate::graph::Gx;

/// Whether probe op `j` of pipeline `q` hands its outer row's cells on unchanged to the ops after it (the module rule).
pub(crate) fn outer_kept(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    let p = &gx.pipelines()[q];
    let Some(Op::Join(JoinDesc::Probe { slot, jointype, catalog: false, .. })) = p.ops.get(j) else { return false };
    if !matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI) {
        return false;
    }
    if gx.end_maker(q) == KindId::JoinDrain || gx.drained_arms(q).is_some() || p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Drain { .. }))) {
        return false;
    }
    let swaps = *jointype == JoinType::JOIN_INNER && gx.life_of(q, *slot).is_some_and(|b| crate::ops::hashjoin::build_side::swaps(gx, b.0 as usize, q, j));
    !swaps && !crate::ops::hashjoin::spill::build::end_takes_pair(gx, q)
}
