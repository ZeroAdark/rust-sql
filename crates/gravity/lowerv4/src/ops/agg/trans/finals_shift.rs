//! Consume the planner's shifted-sum rewrite (A70). Hooks / levers: H34 R2-L09. Budget 160 lines (files.tsv, +25 %
//! gate). Owner M1-FOLD.
//!
//! Road `finals.shift` (`FINALS_SHIFT`; registered correctness-only 09-24; applies under `Fold`): the planner (agg_algebra.rs) has already rewritten `sum(x ± k)` over an
//! integer column into `sum(x)`, `count(x)`, `min(x)`, `max(x)` and a projection `CASE WHEN (min(x) OP k) IS NOT NULL
//! AND (max(x) OP k) IS NOT NULL THEN sum(x) ± k·count(x) END`. The lowerer consumes that plan as written and never
//! redoes the rewrite. The four aggregates fold in the plain end (a window answered from its stats records by
//! answer.stats). The drain stages their finals once. The CASE runs in the next pipeline's projection over those cells,
//! and its `OP` raises PG's 22003 exactly where a row would have. This file holds only the registration: the consumer
//! is the plain end plus expr.core, and e2e v4/m1_fold.sql's shifted sums check it against PG.

crate::strategies! {
    /// Consume the planner's shifted-sum rewrite (A70).
    FINALS_SHIFT = 28, "finals.shift", Fold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(FINALS_SHIFT, &Road)], ..crate::ops::NONE };

/// The road's registration value.
struct Road;
