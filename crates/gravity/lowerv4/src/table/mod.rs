//! `table` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod args;
pub(crate) mod bypass;
pub(crate) mod emit_group;
pub(crate) mod emit_key;
pub(crate) mod emit_runs;
pub mod face;
pub(crate) mod hash_open;
pub(crate) mod key_copy;
pub mod owner_fold;
pub mod keyimg;
pub(crate) mod radix;
pub(crate) mod raw;
pub(crate) mod rec;
pub(crate) mod size;

// The `table` home's face (A-DF1, A-U7: main confirms before group G4 and topn N1 adopt): the keyed record, its fold
// end, its streamed step and its group emit, called directly instead of through the registry. Each records the kind's
// fetch as the registry does (the program's mechanisms-used record stays the same); the registry rows go at A17.
use mcx::PgBox;

use crate::admit::refusal::Variant;
use crate::face::end::EndMech;
use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::px::Px;
use gravity_decompose::ops::shared::agg::GroupDecl;

pub(crate) use emit_group::{W_PTRS, W_STATE};
pub(crate) use rec::GroupRec as Rec;

/// The keyed record of group `g`: its layout, key words and slots.
pub(crate) fn rec<'g, 'p>(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>) -> Rec<'g, 'p> {
    Rec::new(px, g)
}

/// The fold end over pipeline `k` (the hashed group's table, its owner phase).
pub(crate) fn end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    crate::mechs_used::fetched(Variant::Kind(KindId::KeyedHashagg));
    crate::ops::agg::hashagg::make(px, k)
}

/// The fold as step `j` of pipeline `k` (a streamed sorted group over the keyed record).
pub(crate) fn step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    crate::mechs_used::fetched(Variant::Kind(KindId::KeyedHashagg));
    crate::ops::group::stream::make(px, k, j)
}

/// The group emit of pipeline `k`, with HAVING, sorted when a top asks.
pub(crate) fn emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    crate::mechs_used::fetched(Variant::Kind(KindId::KeyedEmitGroup));
    emit_group::make(px, k)
}
