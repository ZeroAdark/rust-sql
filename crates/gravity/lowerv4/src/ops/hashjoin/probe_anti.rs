//! Kind `join.anti` (cards/M7.md; M7-plan §3.3): the ANTI hash probe. Owner M7-PROBE. The first record that passes the
//! join qual drops the outer row (R2); a row with no such record, a NULL key or an empty build goes on as it came (R3,
//! R6: HJ_FILL_OUTER), and the probe never ends the pipeline early (R5). NOT IN is never an anti join in PG 18.6 (a
//! hashed SubPlan: nest.subplan, M8).
//! The step is join.inner's (join/probe_inner.rs, in M1's tree with the walk, join/walk.rs): this row delegates to its
//! maker, which reads the op's jointype (the nest.catalog precedent, V4R-142; M7-plan §1.3 L0-b).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use mcx::PgBox;
use types_nodes::jointype::JoinType;

use crate::face::kinds::KindId;
use crate::face::step::StepMech;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::admit::refusal::Reason;
use crate::frame::iface::kind_impl;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinAnti, &StepMaker(make))], ..crate::ops::NONE };

/// The ANTI probe `j` of pipeline `k`: join.inner's step maker.
fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_ANTI, catalog: false, .. }))) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "join.anti over an op that is not a ANTI probe" });
    }
    (kind_impl::<StepMaker>(&p.node_id, KindId::JoinInner).0)(px, k, j)
}
