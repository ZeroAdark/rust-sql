//! Kind `join.semi` (cards/M7.md; M7-plan §3.2): the SEMI hash probe. Owner M7-PROBE. The first record that passes the
//! join qual hands the outer row on once, with that record's payload (the inner columns read above the join: PG projects
//! single_match's first inner tuple, CNM-40), and ends the walk (R1, R2: PG's single_match, nodeHashjoin.c:578); a
//! row with no such record, or a NULL key, is dropped (R6); an empty build ends the pipeline as an inner join's does
//! (D237). Roads semijoin.marks / semijoin.runs are unbuilt at this level: the probe serves on their straight form (V4R-343).
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

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinSemi, &StepMaker(make))], ..crate::ops::NONE };

/// The SEMI probe `j` of pipeline `k`: join.inner's step maker.
fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_SEMI, catalog: false, .. }))) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "join.semi over an op that is not a SEMI probe" });
    }
    (kind_impl::<StepMaker>(&p.node_id, KindId::JoinInner).0)(px, k, j)
}
