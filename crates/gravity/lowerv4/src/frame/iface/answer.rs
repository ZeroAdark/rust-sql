//! A scan source's questions about the plain fold that ends its pipeline, read through the interface home (lint RA):
//! the source asks whether the fold's answer can be served from its own lanes, and the agg family's trans strategies
//! answer, so no scan family imports agg's internals.
use crate::frame::px::Px;

/// The counted qual of pipeline `k`'s plain fold, asked by its scan source (ops/agg trans strategies/answer_counted.rs
/// `qual_at`): filter op `j`'s qual `i` is a predicate the integer lane's granule record tallies (`summary_tally`) over a
/// lane of `site`, every aggregate a count; `(j, i, x's site lane, the predicate)`.
pub(crate) fn answer_counted_qual(px: &Px<'_, '_>, k: usize, site: &gravity_program::SiteDesc, zoned: u64) -> Option<(usize, usize, usize, SummaryTally)> {
    crate::ops::agg::trans::strategies::answer_counted::qual_at(px, k, site, zoned)
}

/// The pgrc granule summary's tally (ops/scan pgrc summary.rs): a predicate over an integral lane whose passing rows a
/// granule record counts, asked by the plain fold's counted answer.
pub(crate) use crate::ops::scan::pgrc::summary::Tally as SummaryTally;

/// Qual `q` as a tallied predicate (ops/scan pgrc summary.rs `tally`): its column's attno and form.
pub(crate) fn summary_tally(q: types_nodes::Node<'_>) -> Option<(i16, SummaryTally)> {
    crate::ops::scan::pgrc::summary::tally(q)
}

/// A FILTER's tallied predicate (ops/scan pgrc summary.rs `filter_tally`).
pub(crate) use crate::ops::scan::pgrc::summary::FilterTally as SummaryFilterTally;

/// Aggregate FILTER `q` as a tallied predicate (ops/scan pgrc summary.rs `filter_tally`), `col` mapping a Var of the fold's
/// row to its scan column and type.
pub(crate) fn summary_filter_tally(q: types_nodes::Node<'_>, col: &dyn Fn(types_nodes::Node<'_>) -> Option<(i16, u32)>) -> Option<(i16, SummaryFilterTally)> {
    crate::ops::scan::pgrc::summary::filter_tally(q, col)
}

/// Declares pipeline `k`'s plain-fold answer lanes on its scan source's `site` over the ops `zoned` covers (ops/agg trans
/// answer_site.rs `declare_site`); whether the fold takes the answer.
pub(crate) fn answer_declare_site(px: &mut Px<'_, '_>, k: usize, site: &mut gravity_program::SiteDesc, zoned: u64) -> bool {
    crate::ops::agg::trans::answer::declare_site(px, k, site, zoned)
}

/// Whether pipeline `k`'s end is a plain fold of `count(*)`s under no FILTER that takes windows whole (ops/agg trans
/// answer.rs `count_star_only`), asked by its scan source for decode.unread.
pub(crate) fn answer_count_star_only(px: &Px<'_, '_>, k: usize) -> bool {
    crate::ops::agg::trans::answer::count_star_only(px, k)
}

/// D3042: whether pipeline `k`'s end is the fold's `count_only` plan (answer.rs `plan`: `count_star_only` over
/// projections that keep every row, plain.rs `rows_kept`), every window counted `end - cursor`, asked by its scan source
/// for count.only.
pub(crate) fn answer_count_only(px: &Px<'_, '_>, k: usize) -> bool {
    crate::ops::agg::trans::answer::count_star_only(px, k) && crate::ops::agg::trans::plain::rows_kept(px, k)
}
