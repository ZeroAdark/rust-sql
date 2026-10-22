//! Sort's run over given keys, as another family builds on it: a Merge Append's breaker form reads its arms into sort's
//! resident run over the merge keys and emits that run in key order (ops/append/merge.rs), so the merge names sort here
//! only (lint RA).
use gravity_decompose::ops::shared::sort::SortKey;
use mcx::PgBox;
use types_nodes::list::NodeList;

use crate::face::end::EndMech;
use crate::face::source::SourceMech;
use crate::frame::px::Px;

/// A producer's run roads as given (ops/sort/lower.rs `Arm`).
pub(crate) use crate::ops::sort::lower::Arm as SortArm;

/// Sort's producer end over `keys` for pipeline `k` (ops/sort/lower.rs `make_end_over`).
pub(crate) fn sort_end_over<'g>(px: &mut Px<'g, '_>, k: usize, keys: &[SortKey], cols: &NodeList<'_>, arm: SortArm) -> PgBox<'g, dyn EndMech + 'g> {
    crate::ops::sort::lower::make_end_over(px, k, keys, cols, arm)
}

/// Sort's emit of the run over `keys` that pipeline `k` reads (ops/sort/lower.rs `make_emit_over`).
pub(crate) fn sort_emit_over<'g>(px: &mut Px<'g, '_>, k: usize, keys: &[SortKey], cols: &NodeList<'_>) -> PgBox<'g, dyn SourceMech + 'g> {
    crate::ops::sort::lower::make_emit_over(px, k, keys, cols)
}

/// Sort's run of full sort `producer` into `slot`, read as a cursor by a pull merge's reader at its consumed index
/// `i` (ops/sort/lower.rs `RunEmit::over_at`), its words keyed by the arm pipeline `arm`.
pub(crate) fn sort_cursor<'g>(px: &mut Px<'g, '_>, arm: usize, producer: usize, slot: u8, i: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    mcx::pg_box_dyn!(px.mcx(), crate::ops::sort::lower::RunEmit::over_at(px, arm, producer, slot, i) => dyn SourceMech + 'g)
}

/// The row full sort `producer` leaves in its run (ops/sort/lower.rs `sort_of`: its target list).
pub(crate) fn sort_cols<'g, 'p>(px: &Px<'g, 'p>, producer: usize) -> &'g NodeList<'p> {
    crate::ops::sort::lower::sort_of(px, producer).1
}

/// The resident merge's protocol for producer `k` into `slot` (ops/sort/strategies/resident.rs `declare`): a fixed row names
/// its spec and key words.
pub(crate) fn sort_resident(px: &mut Px<'_, '_>, k: usize, slot: u8, fixed: Option<(u64, u16)>) {
    crate::ops::sort::strategies::resident::declare(px, k, slot, fixed)
}

/// D449: whether full sort `producer`, whose run arm pipeline `arm` alone reads, may hold its fill's error (op 14 for
/// runs.sort, the program's `held_sort`) until a pull merge's reader first reads that run's end, PostgreSQL's first pull of
/// the Sort arm (ops/mergejoin/held_inner.rs `fill_shape`: builtin code over a scan, quiet feeders, no rerun set).
pub(crate) fn held_fill(gx: &crate::graph::Gx<'_, '_>, arm: usize, producer: usize) -> bool {
    crate::ops::mergejoin::held_inner::fill_shape(gx, arm, producer, &[producer]).is_some()
}
