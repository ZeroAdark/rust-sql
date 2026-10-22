//! The srf family's visible decisions, in the effects home (lint rule RV; moved from ops/srf/reopen_once.rs):
//! `volatile`, D1657's once rule's one VOLATILE test (effects' reader, with D1907's domain checks: effects.rs `check_acts`;
//! srf/reopen_once.rs).
use gravity_decompose::effects;
use gravity_program::ArenaRef;
use types_nodes::Node;

use crate::graph::Gx;

/// D1657's VOLATILE test: `n` acts by effects' reader (`DECISIONS.md` 3: a VOLATILE call or one the catalog cannot
/// answer, a SubPlan, whose plan lowering cannot read, and any node the walk does not know), or D1907: it holds a domain
/// coercion whose CHECK acts, since the inline forms run the check at every open (effects reads a domain coercion as one
/// that may raise, not one that acts): effects.rs `check_acts`.
pub(crate) fn volatile<'p>(gx: &Gx<'_, 'p>, arena: ArenaRef, n: Node<'p>) -> bool {
    let cx = effects::Cx::without_stmt(gx.facts, arena);
    gx.fx(n).acts_plain(&cx) || gx.fx(n).check_acts(&cx)
}
