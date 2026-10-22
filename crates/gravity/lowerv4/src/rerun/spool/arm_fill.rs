//! rowsecurity#343 (material): the later arms of a streamed write whose nest copies read one plan Material (readers.rs
//! `copies`) wait on the fill the first arm's nest deferred (lazy.rs `declare` pushed its group, lazy for that arm). The
//! fill's slot stays pending until it runs, so a later arm's first outer row stops at its own nest step on its own
//! consumed index, and the driver runs the group there when no earlier arm did (driver entry.rs `lazy_build`: a second
//! reader of a pending lazy slot); once it ran, the bit is clear and every later arm reads the record from its start.
use crate::frame::px::Px;
use crate::rerun::spool::lazy::Lazy;

/// A later arm's nest at graph pipeline `k` over fill `kf` in `slot`: its wait when the fill went lazy for the first arm
/// (`loud`: a deferred fill, read whole), else `None` (the fill ran in pipeline order before every arm).
pub(crate) fn later(px: &mut Px<'_, '_>, k: usize, kf: usize, slot: u8, loud: bool) -> Option<Lazy> {
    if !px.lazy.iter().any(|&(x, _)| x == kf) {
        return None;
    }
    let consumed = px.consumed(k, slot)?;
    if loud {
        px.whole_read(k);
    }
    Some(Lazy::new(consumed))
}

/// rowsecurity#342 / #343: the union pipeline `e` emits, settled under the plan Material that `e` fills and the nest copies
/// of a streamed write's arms read (readers.rs `copies`), whose fill takes the deferred form (form.rs `deferred`, D1108
/// row 3 by `whole_first_arms`): lazy.rs runs the feeds and the emit as one group at the first outer row that reaches any
/// arm's nest, the arms one after another in plan order, or never. PostgreSQL's Materialize reads its Append to its end
/// inside that row's first inner scan (a non-unique INNER or LEFT nest with no parameters reads the whole inner), arm after
/// arm, so the arms' calls and errors are PostgreSQL's; nothing between them shows (the nest's qual is quiet and the arm's
/// ops after the nest are calm, `whole_first_arms`). union_arms.rs `admit` serves such a union to its reader (one call line).
pub(crate) fn whole_union(gx: &crate::graph::Gx<'_, '_>, e: usize) -> bool {
    let ps = gx.pipelines();
    e < ps.len() && crate::rerun::union_arms::union_of(&ps[e]).is_some() && crate::rerun::spool::readers::copies(gx, e).is_some() && crate::rerun::spool::form::deferred(gx, e)
}
