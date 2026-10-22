//! The pgrc source's effect decisions: the quiet reading its lead-qual verdicts ask (ops/scan/pgrc/lower.rs
//! `declare_verdicts`: a zone, bloom, PSMA, code-sparse or code-range verdict, or a condition-cache key, is declared for
//! a lead qual only while every qual before it raises on no row, because the verdict skips rows PostgreSQL runs those
//! quals on; ExecQual (executor/executor.h) runs a row's quals in order, up to the first false).
use types_nodes::Node;

use crate::graph::Gx;

/// D454: whether lead qual `n` raises on no row: `raise_free` (its LIKE / ILIKE / regex leaf is the one pattern rule,
/// decompose `effects::quiet_pattern`, DC-14; exec front.rs keys the pattern's class), or a comparison the effects home
/// reads quiet past PostgreSQL's leakproof mark (`errorhold::quiet_cmp`: `effects::quiet_call`, e.g. numeric_lt). D2875:
/// a PARAM_EXEC `init_unseen` passes is a bound word.
pub(crate) fn quiet_qual(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    let unseen = |id: i32| init_unseen(gx, id);
    crate::effects::raise_free_by(gx, n, &unseen, false) || super::errorhold::quiet_cmp_by(gx, n, &unseen)
}

/// D3126: whether lead qual `n` may raise only inside a comparand that reads no column: a quiet comparison (as
/// `quiet_qual`'s) whose every argument raises on no row or reads no column. Such a comparand is the STABLE one pgrc's
/// open evaluates (verdict_zone.rs `stable_of`): an open whose evaluation of it raises nothing proves no row raises in the
/// qual, so a later qual's open-time verdict may skip rows PostgreSQL would run this qual on. The home's answer,
/// `Fx::raises_in_comparand`.
pub(crate) fn raises_in_comparand(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).raises_in_comparand(&|id| init_unseen(gx, id))
}

/// D2875: PARAM_EXEC `id` is set by InitPlans only, none of which acts or may raise
/// (`Effects::of_init_plan`: decompose F5, a second row's 21000 included), so running one before the row PostgreSQL
/// first reads the parameter on, or never where a verdict skips that row, shows nothing. PS1-05: also road
/// `nest.eagersafe`'s test (pgrc verdict_zone.rs `init_of`: such a value feeds the scan's open-time zone verdicts).
pub(crate) fn init_unseen(gx: &Gx<'_, '_>, id: i32) -> bool {
    let mut sets = gx.graph.init_plans.iter().filter(|i| i.params.iter().any(|p| p == id)).peekable();
    sets.peek().is_some() && sets.all(|i| {
        let e = gravity_decompose::effects::Effects::of_init_plan(i);
        !e.acts && !e.may_raise
    })
}
