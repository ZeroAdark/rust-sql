//! Kind `join.full` (M7-plan §3.5; L2; cards/M7.md). Owner M7-DRAIN. A delegating maker (the module rule):
//! join.inner's probe step, whose walk marks as RIGHT does and fills the unmatched outer row as LEFT does (join/walk.rs,
//! probe_fill.rs); the drain (join/drain.rs) sends the unmarked records with the outer side NULL.
use mcx::PgBox;

use crate::face::kinds::KindId;
use crate::face::step::StepMech;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::frame::iface::kind_impl;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinFull, &StepMaker(make))], ..crate::ops::NONE };

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (kind_impl::<StepMaker>(&node, KindId::JoinInner).0)(px, k, j)
}
