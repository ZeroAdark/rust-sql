//! A write's recheck admission (C4, R83 / D2050): an UPDATE / DELETE / MERGE over a join reruns, at an EvalPlanQual
//! recheck, the producers its REFERENCE / COPY marks name (marks.rs `sites_in`). Lockrows rerun's `verdict` answers for
//! its road (`Road::Write`) as for a row lock's: refused where the rerun could raise, call or show what PostgreSQL's
//! EvalPlanQual does not.
use gravity_decompose::ops::shared::modifytable::RowMark;

use crate::admit::refusal::Reason;
use crate::frame::px::Px;
use crate::ops::lockrows::rerun::{epq_lazy, verdict, Road};

/// Refuses recheck pipeline `k` of a write over result relations `targets` (range table indexes), its marks `marks`,
/// where `verdict` answers a refusal. The lazy group of `k`'s epq probe (`epq_lazy`) runs at that probe's first row in
/// any run, the recheck's too; the row lock's held builds (`first_held`, `epq_held`) are not held on this road. Its
/// InitPlans' members (marks.rs `init_members`) are never rerun.
pub(crate) fn admit(px: &Px<'_, '_>, k: usize, node: i32, marks: &[RowMark], targets: &[u32]) {
    let lazy = epq_lazy(px, k);
    let init = super::marks::init_members(px);
    if let Some(why) = verdict(px.gx, k, marks, Road::Write { targets, arena: px.arena, init: &init }, &lazy) {
        crate::refuse!(node, Reason::Shape { why });
    }
}
