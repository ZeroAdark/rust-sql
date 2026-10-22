//! A read-only reader of a btree's separator keys (its pivot tuples): the boundaries that cut the key space into the
//! ranges of one level's children. A caller cuts a scan into key ranges at them (gravity's ordered key-range claims);
//! nothing here changes the index or takes a predicate lock.
//!
//! On a non-leaf page every data item but the first carries a separator key (its child holds the keys from it up to the
//! next one), the first data item's key is minus infinity (its lower bound is the left sibling's high key), and the
//! high key of a page that is not the rightmost of its level is the separator between that page's last child and the
//! right sibling's first. So one level's separators, in key order, are each page's data items past its first, then the
//! page's high key, page by page from the leftmost.
//!
//! The walk holds one page's read lock at a time and moves right by the right-link it read under that lock, as
//! `_bt_get_endpoint`'s callers do. A split or a page deletion that runs meanwhile may leave a separator out or keep a
//! stale one: the separators stay in ascending order, and any set of ascending boundaries still covers the whole key
//! space, so a stale one only skews the ranges' sizes. Half-dead and deleted pages are skipped (`P_IGNORE`).
use ::types_error::PgResult;
use ::types_nbtree::{P_FIRSTDATAKEY, P_HIKEY, P_IGNORE, P_RIGHTMOST, BT_READ};

use crate::itup::index_tuple_size;
use crate::page::{bt_relandgetbuf, bt_relbuf, page_item, page_opaque};
use crate::readrel::BtReadRel;

/// The separators of one btree level: `level` (1 = the parents of the leaves) and its separator tuples in ascending key
/// order, each a copy of the on-page pivot tuple (its key attributes may be truncated by suffix truncation, and it may
/// carry a heap TID as its last key attribute; read the attributes with `itup::index_getattr` over the index's
/// descriptor and `itup::bt_tuple_get_natts` for the count kept).
#[derive(Debug, Default)]
pub struct Pivots {
    pub level: u32,
    pub keys: Vec<Vec<u8>>,
}

/// The separators of the highest level that cuts the key space into at least `min` ranges (its separators number at
/// least `min - 1`), else those of level 1 (every leaf's lower boundary). Empty when the index is empty or its root is
/// a leaf (one page: no separator).
pub fn bt_pivots<R: BtReadRel + ?Sized>(rel: &R, min: usize) -> PgResult<Pivots> {
    let Some(root) = crate::page::bt_gettrueroot(rel)? else { return Ok(Pivots::default()) };
    let top = page_opaque(&root.page()).btpo_level;
    bt_relbuf(rel, root)?;
    let mut level = top;
    while level > 0 {
        let keys = level_separators(rel, level)?;
        if keys.len() + 1 >= min || level == 1 {
            return Ok(Pivots { level, keys });
        }
        level -= 1;
    }
    Ok(Pivots::default())
}

/// Level `level`'s separators, left to right.
fn level_separators<R: BtReadRel + ?Sized>(rel: &R, level: u32) -> PgResult<Vec<Vec<u8>>> {
    let Some(mut pin) = crate::search::bt_get_endpoint(rel, level, false)? else { return Ok(Vec::new()) };
    let mut out: Vec<Vec<u8>> = Vec::new();
    loop {
        let next = {
            let page = pin.page();
            let opaque = page_opaque(&page);
            if !P_IGNORE(&opaque) && opaque.btpo_level == level {
                let first = P_FIRSTDATAKEY(&opaque);
                for off in first + 1..=page.max_offset_number() {
                    out.push(copy(page_item(&page, page.item_id(off)))?);
                }
                if !P_RIGHTMOST(&opaque) {
                    out.push(copy(page_item(&page, page.item_id(P_HIKEY)))?);
                }
            }
            (!P_RIGHTMOST(&opaque)).then_some(opaque.btpo_next)
        };
        match next {
            Some(blkno) => pin = bt_relandgetbuf(rel, Some(pin), blkno, BT_READ)?,
            None => {
                bt_relbuf(rel, pin)?;
                return Ok(out);
            }
        }
    }
}

/// The bytes of the pivot tuple at `itup` (a live item of a pinned, read-locked page).
fn copy(itup: crate::itup::ITup) -> PgResult<Vec<u8>> {
    // SAFETY: `itup` points at a live index tuple of the pinned, locked page; its size is its own header's
    let src = unsafe { core::slice::from_raw_parts(itup, index_tuple_size(itup)) };
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(src.len()).map_err(|_| ::mcx::oom_named("nbtree", src.len()))?;
    out.extend_from_slice(src);
    Ok(out)
}
