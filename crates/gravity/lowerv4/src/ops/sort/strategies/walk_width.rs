//! Road `width.walk` (cards/M3.md L7, N10; an A/B road: dropped with a row if its group-walk twin loses): a walk round's width is
//! the work in it. The walk's protocol (exec `v4/m3_walk.rs`) sizes each claim from the residency of the next granules'
//! extents (hosts `extent_resident`, Decision 11: a data fact, never a timer): a resident round hands several granules
//! per claim, a round that is not resident one granule per claim, and the section's width never exceeds the granules the
//! walk can still hand out. The width never changes the rows: every claim is still whole granules in key order, stopped at
//! the same bound. runs/walk.rs reads this road while it declares the walk (`arg`); W0 is untouched (one leader claims
//! alone). Owner M3-WALK. Budget 160 lines.
use crate::frame::px::Px;

crate::strategies! {
    /// Walk sections on fewer workers.
    WIDTH_WALK = 68, "width.walk", Walkable;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(WIDTH_WALK, &WALK_WIDTH)], ..crate::ops::NONE };

/// `width.walk`'s row: its reader is runs/walk.rs's declaration.
pub(crate) struct WalkWidth;
static WALK_WIDTH: WalkWidth = WalkWidth;

/// The walk protocol's width bit (runs/walk.rs `WALK_WIDTH`), read through this road's row.
pub(crate) fn arg(px: &Px<'_, '_>) -> u64 {
    let node = px.gx.root_node();
    let _: &WalkWidth = crate::frame::iface::need_strategy::<WalkWidth>(&node, WIDTH_WALK);
    crate::ops::sort::strategies::walk::WALK_WIDTH
}
