//! A grouped top's questions about the hash-aggregate end that folds its groups, read through the interface home (lint
//! RA): topn's two-pass and parted strategies build their pass ends as the agg family's keyed end makes them
//! (ops/agg hashagg.rs `make` / `end_of`) and read an aggregate's fold class (agg trans class_of.rs `class_in`); the
//! keyed end asks the two-pass strategies for a HAVING split's end, pass 2's admission and its probe
//! (ops/sort/topn/strategies/twopass.rs). Neither family imports the other's internals; the passes' data is
//! ops/shared/twopass.rs.
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl};
use gravity_ir::builder::FunctionBuilder;
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::face::fold::FoldClass;
use crate::ops::shared::twopass::{Admit, AdmitLive};
use crate::table::keyimg::ImageWords;
use crate::table::rec::GroupRec;

/// Pipeline `k`'s keyed hash end as the agg family makes it for a one-pass group (ops/agg hashagg.rs `make`).
pub(crate) fn group_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    crate::ops::agg::hashagg::make(px, k)
}

/// Pipeline `k`'s hash end over record `rec`, probing `admit`, in pass `split`, through the per-code front `pc` (ops/agg
/// hashagg.rs `end_of`).
pub(crate) fn group_end_of<'g, 'p>(
    px: &mut Px<'g, 'p>,
    k: usize,
    rec: GroupRec<'g, 'p>,
    admit: Option<crate::ops::shared::twopass::Admit>,
    split: Option<crate::ops::shared::twopass::Split>,
    pc: Option<crate::dict::strategies::codefront_percode::Percode>,
) -> PgBox<'g, dyn EndMech + 'g> {
    crate::ops::agg::hashagg::end_of(px, k, rec, admit, split, pc)
}

/// Aggregate `a`'s fold class in an end that folds num.window trees (`trees`) or none (agg trans class_of.rs `class_in`).
pub(crate) fn fold_class_in(cx: &impl crate::admit::refusal::Site, a: &AggDecl<'_>, trees: bool, mcx: mcx::Mcx<'_>) -> &'static dyn FoldClass {
    crate::ops::agg::trans::class_of::class_in(cx, a, trees, mcx)
}

/// The keyed end of pipeline `k` for group `g` split into two passes by the count its one top orders first, when
/// twopass.count takes it (ops/sort/topn/strategies/twopass_count.rs `split`).
pub(crate) fn twopass_count_split<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    crate::ops::sort::topn::strategies::twopass_count::split(px, k, g, rec)
}

/// The keyed end of pipeline `k` for group `g` split by its HAVING into two passes, when twopass.having takes it
/// (ops/sort/topn/strategies/twopass.rs `having_split`).
pub(crate) fn twopass_having_split<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, g: &'g GroupDecl<'p>, rec: &GroupRec<'g, 'p>) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    crate::ops::sort::topn::strategies::twopass::having_split(px, k, g, rec)
}

/// Pass 2's admission when pipeline `k` folds the planner's pass 2 of group `g` (twopass.rs `planner_pass2`).
pub(crate) fn twopass_pass2(px: &mut Px<'_, '_>, k: usize, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>) -> Option<Admit> {
    crate::ops::sort::topn::strategies::twopass::planner_pass2(px, k, g, rec)
}

/// D3148: the admission a finished hash build's keys give the keyed end of pipeline `k` for group `g`, when the join
/// above the group probes that build on a group key (ops/hashjoin/strategies/domain.rs `admit`, road join.domain).
pub(crate) fn join_domain(px: &mut Px<'_, '_>, k: usize, g: &GroupDecl<'_>, rec: &GroupRec<'_, '_>) -> Option<Admit> {
    crate::ops::hashjoin::strategies::domain::admit(px, k, g, rec)
}

/// Pass 2's probe of a group's image `img` against the admitted set (twopass.rs `admitted_at`): the block a group that is
/// not admitted joins. `live`: the set's header words its row loop read (D3326).
pub(crate) fn twopass_admitted_at(admit: Option<Admit>, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, img: &ImageWords, live: Option<AdmitLive>) -> Option<gravity_ir::Block> {
    crate::ops::sort::topn::strategies::twopass::admitted_at(admit, b, env, rec, img, live)
}
