pub mod block;
pub mod builtins;
pub mod image;
pub mod pagesrc;
pub mod tomb;
pub mod view;
pub mod vis;

pub use block::{BlockImage, Hdr, Kind};
pub use image::{current, images, CompanionImage, Images, NoImage};
pub use pagesrc::{BufPages, PageSource, Procs, TxnOracle};
pub use view::{view, ColView, SegSource, SegView, ViewFilter};
pub use vis::{Mvcc, SnapView};

pub fn locator(rel: &types_rel::Relation<'_>) -> pgrc_marks::Locator {
    let l = rel.rd_locator.get();
    (l.spcOid, l.dbOid, l.relNumber)
}

/// `None`: no image (the cap 0 or over it, a temporary relation), which [`pgrc_front::front_bearing_reads`] reads as bearing.
pub fn tomb_lock_only<'mcx>(tomb: &types_rel::Relation<'mcx>) -> types_error::PgResult<Option<bool>> {
    if tomb.uses_local_buffers() || pgrc_read::mem::cap() == 0 {
        return Ok(None);
    }
    match image::current(locator(tomb), tomb.rd_id, Kind::Tomb, &BufPages { rel: tomb }, &Procs { rel: tomb })? {
        Ok(img) => Ok(Some(img.live == 0)),
        Err(_) => Ok(None),
    }
}

pub fn init_seams() {
    pgrc_front_seams::tomb_lock_only::set(tomb_lock_only);
    pgrc_front_seams::statement_deletes::set(view::deletes);
}
