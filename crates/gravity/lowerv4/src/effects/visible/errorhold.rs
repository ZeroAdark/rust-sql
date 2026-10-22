//! Comparisons the effects home reads quiet past PostgreSQL's leakproof mark.
//! - `quiet_cmp`: D393, a hash join drain arm's comparison.
use types_nodes::Node;

use crate::graph::Gx;

/// D393: a comparison the effects home reads quiet though PostgreSQL does not mark it leakproof
/// (`effects::quiet_call`: a leakproof builtin or `types_core::quiet::call`, e.g. numeric_gt), over arguments that raise
/// nothing (the join filter `e.e_amt > 57` on a numeric column).
pub(crate) fn quiet_cmp(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    quiet_cmp_by(gx, n, &|_| false)
}

/// `quiet_cmp` with the PARAM_EXEC ids `quiet` names bound words (`effects::raise_free_by`). D3932: only under a set
/// input collation (the home's `collation_set`): two arguments of different implicit collations leave a collation-aware
/// comparison none, and it raises at its first row (varlena.c check_collation_set). The home's answer, `Fx::quiet_cmp`.
pub(crate) fn quiet_cmp_by(gx: &Gx<'_, '_>, n: Node<'_>, quiet: &dyn Fn(i32) -> bool) -> bool {
    gx.fx(n).quiet_cmp(quiet)
}
