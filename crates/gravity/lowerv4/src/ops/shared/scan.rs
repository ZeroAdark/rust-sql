//! The source face's compile-time state other families read (an aggregate end counting a covered window whole), and
//! road `verdict.rtconst`'s gate (scan/pgrc/verdict_rtconst.rs): whether a hash probe's pgrc scan may skip the granules
//! no build key can match (join/rtfilter.rs asks it, for a swapped pair too).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Am, Op, Source};
use types_nodes::jointype::JoinType;

use crate::face::kinds::Face;
use crate::face::source::VerdictKind;
use crate::frame::px::{ExId, Px};

/// The source face's compile-time state (`Px::ext_mut(Face::Source)`): each leading qual's place in its pipeline
/// `(qual, k, op j, list index i)`, so a verdict road reads the plan node behind an `ExId`.
#[derive(Default)]
pub(crate) struct SourceExt {
    pub quals: Vec<(ExId, usize, usize, usize)>,
    /// decode.unread: the pipelines whose end counts a covered window whole (fold/answer.rs `count_covered`).
    pub whole: Vec<usize>,
    /// Each pgrc pipeline's covered leading filter ops (`SourceMech::covers`: bit j, op j), when any.
    pub covers: Vec<(usize, u64)>,
}
crate::frame::px::ext_g!(SourceExt);

/// Pipeline `k`'s end may count a covered window whole: its source covers every filter op (`declare_verdicts`).
pub(crate) fn counts_covered(px: &Px<'_, '_>, k: usize) -> bool {
    px.ext::<SourceExt>(Face::Source).is_some_and(|e| e.whole.contains(&k))
}

/// Pipeline `k`'s covered leading filter ops (bit j: op j; `declare_verdicts`): in a window whose `WinConds::covered`
/// holds, every row of its selection passes them, the frame eliding their steps. 0: none (no pgrc source of its own).
pub(crate) fn covered_ops(px: &Px<'_, '_>, k: usize) -> u64 {
    px.ext::<SourceExt>(Face::Source).and_then(|e| e.covers.iter().find(|c| c.0 == k)).map_or(0, |c| c.1)
}

/// Whether pipeline `q`'s op `j`, a hash probe, may have its pgrc scan skip the granules no build key can match: the
/// road is on and the probe drops a row without a match (the probe is the plan's outer side).
pub(crate) fn accepts(px: &Px<'_, '_>, q: usize, j: usize) -> bool {
    let Some(p) = px.gx.pipelines().get(q) else { return false };
    let pgrc = matches!(p.source, Source::Range { am: Am::Pgrcolumnar, .. });
    let drops = matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { jointype, .. })) if drops_unmatched(*jointype));
    VerdictKind::RtConst.on() && pgrc && drops
}

/// join.buildside's swapped pair (hashjoin/build_side.rs `swaps`: an INNER pair whose every op and key is raise-free):
/// whether the scan of graph pipeline `k`, the build side the swap made the probe, may skip the granules no key of the
/// swapped build can match: the road is on and `k`'s source is a pgrc scan (the pair is INNER: an unmatched row never
/// reaches the output).
pub(crate) fn accepts_swapped(px: &Px<'_, '_>, k: usize) -> bool {
    px.gx.pipelines().get(k).is_some_and(|p| matches!(p.source, Source::Range { am: Am::Pgrcolumnar, .. })) && VerdictKind::RtConst.on()
}

/// A probe (outer) row without a match never reaches the output (join.domain's join types too).
pub(crate) fn drops_unmatched(jt: JoinType) -> bool {
    matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_probe_that_drops_its_unmatched_rows_skips() {
        use JoinType::*;
        for jt in [JOIN_INNER, JOIN_SEMI, JOIN_RIGHT, JOIN_RIGHT_SEMI, JOIN_RIGHT_ANTI] {
            assert!(drops_unmatched(jt), "{jt:?}");
        }
        for jt in [JOIN_LEFT, JOIN_FULL, JOIN_ANTI, JOIN_UNIQUE_OUTER, JOIN_UNIQUE_INNER] {
            assert!(!drops_unmatched(jt), "{jt:?}");
        }
    }
}
