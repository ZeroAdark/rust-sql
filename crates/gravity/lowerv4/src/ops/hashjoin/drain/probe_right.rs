//! Kind `join.right` (RIGHT / RIGHT SEMI / RIGHT ANTI; M7-plan §3.4; L2; cards/M7.md). Owner M7-DRAIN. A delegating
//! maker (the module rule): join.inner's probe step, whose walk marks each record a joined row passed the
//! join qual on (join/walk.rs's right-family arms: RIGHT hands the row on, RIGHT SEMI and RIGHT ANTI hand nothing on) over
//! a build laid with its marks (L0-d, join/build.rs); the drain (join/drain.rs) sends the records after the probe.
use mcx::PgBox;

use crate::face::kinds::KindId;
use crate::face::step::StepMech;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::frame::iface::kind_impl;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinRight, &StepMaker(make))], ..crate::ops::NONE };

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (kind_impl::<StepMaker>(&node, KindId::JoinInner).0)(px, k, j)
}
