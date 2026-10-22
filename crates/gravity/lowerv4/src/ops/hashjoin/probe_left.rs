//! Kind `join.left` (cards/M7.md; M7-plan §3.1): the LEFT hash probe. Owner M7-PROBE. PG's HJ_FILL_OUTER: an outer row
//! whose walk hands nothing on (a NULL key, an empty build, no record passing the join qual) goes on once with the inner
//! columns NULL (R3, R4); every qual-passing record goes on joined (R1), and the probe never ends the pipeline early
//! (R5). An outer-first probe (nodeHashjoin.c:351): its build waits for the first row (op 14, D824; build_class.rs L0-e).
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

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinLeft, &StepMaker(make))], ..crate::ops::NONE };

/// The LEFT probe `j` of pipeline `k`: join.inner's step maker.
fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, catalog: false, .. }))) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "join.left over an op that is not a LEFT probe" });
    }
    (kind_impl::<StepMaker>(&p.node_id, KindId::JoinInner).0)(px, k, j)
}
