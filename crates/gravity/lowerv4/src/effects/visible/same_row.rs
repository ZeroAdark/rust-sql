//! Construct C2's same-row rule: where code that may raise runs ahead of PostgreSQL 18.6's demand, it runs at
//! PostgreSQL's own point, with no departure to license, only when its error reaches the client at the same output row
//! as PostgreSQL's. The one decider for every site that claims that point (ops/ctescan's fill ahead of its readers,
//! rerun/store_read.rs's FROM-call SubPlans at the open); a site with any term false keeps its pulled form or its
//! refusal.

/// The four terms of the same-row rule, each as the site reads it.
pub(crate) struct SameRow {
    /// The early work's rows are read whole: PostgreSQL's demand reads the same rows, so the early run makes no row
    /// PostgreSQL never makes (no LIMIT, gate or early stop over a reader).
    pub(crate) whole: bool,
    /// Something PostgreSQL evaluates before the early work acts (a VOLATILE call, user code, a write).
    pub(crate) acts_before: bool,
    /// Something PostgreSQL evaluates before the early work may raise: its error would come first in PostgreSQL.
    pub(crate) raises_before: bool,
    /// A row may reach the client before PostgreSQL's point (a named cursor's FETCH streaming a reader, a UNION ALL arm
    /// or an earlier pipeline that streams first): PostgreSQL returns that row before the error.
    pub(crate) row_first: bool,
}

/// The early work runs at PostgreSQL's point: the same first error at the same output row.
pub(crate) fn at_pg_point(t: &SameRow) -> bool {
    t.whole && !t.acts_before && !t.raises_before && !t.row_first
}
