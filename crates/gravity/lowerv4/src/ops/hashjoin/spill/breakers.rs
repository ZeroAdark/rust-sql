//! Kind `spill.breakers` (the Build / KeptOuter / Pairs Grace spill of a hash build past its grant; HashAgg / SortRuns
//! register the existing spills; D365; L3) (cards/M7.md, M7-changes.md). Owner M7-SPILL. B3 (V4R-359): the
//! kind's one mechanism delegates a hash build's spill to join.inner's (join/build_spill.rs, M1 scope), so the M1 build
//! end and probe reach it through the face; the other forms refuse by name until B4 and D365 build them.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use crate::face::kinds::KindId;
use crate::face::spill::{SpillDecl, SpillImpl, SpillMech};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::stop::Exits;
use crate::frame::iface::kind_impl;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SpillBreakers, &SpillImpl(&DELEGATE))], ..crate::ops::NONE };

struct Delegate;
static DELEGATE: Delegate = Delegate;

impl SpillMech for Delegate {
    fn spill_row(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _d: &SpillDecl, _h: Value, _row: Value, _bytes: Value) {
        crate::not_built!(env.node, crate::admit::refusal::Variant::Kind(KindId::SpillBreakers))
    }

    fn reload(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _d: &SpillDecl, _x: &Exits) {
        crate::not_built!(env.node, crate::admit::refusal::Variant::Kind(KindId::SpillBreakers))
    }

    fn build_bits(&self, px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Option<u8> {
        let node = px.gx.pipelines()[k].node_id;
        // MECHCORPUS: a borrowed helper, not a join (mechs_used.rs)
        crate::mechs_used::probe(crate::admit::refusal::Variant::Kind(KindId::JoinInner), kind_impl::<SpillImpl>(&node, KindId::JoinInner).0.build_bits(px, k, q, j))
    }
}
