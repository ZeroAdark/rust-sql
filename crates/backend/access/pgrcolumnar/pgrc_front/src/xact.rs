//! The statement buffer. A COPY's or a ModifyTable statement's rows wait here as heap tuple images; statement end
//! writes a buffer below the bulk threshold into the active segment, and a statement that reaches the threshold or
//! [`BUFFER_BYTES`] replays its rows into the bulk lane and sends the rest there.

use std::cell::RefCell;

use types_core::{Oid, BLCKSZ};
use types_error::PgResult;
use types_tuple::ItemPointerData;

use crate::route::bulk_rows;

/// Half a rotation's worth of heap pages (16 MiB): a buffer that big fills a good part as well as a seal would, and a
/// statement never pushes a segment far past the rotation threshold.
pub const BUFFER_BYTES: usize = crate::maint::ROTATE_BLOCKS as usize * BLCKSZ / 2;

/// Heap tuple images in insertion order, each 8-byte aligned (a MAXALIGN'd header).
#[derive(Default)]
pub struct Rows {
    words: Vec<u64>,
    spans: Vec<(usize, u32)>,
    bytes: usize,
}

impl Rows {
    pub fn push(&mut self, image: &[u8]) -> PgResult<()> {
        let at = self.words.len();
        ::mcx::stdvec::try_reserve_more(&mut self.spans, 1, "pgrc statement buffer")?;
        ::mcx::stdvec::try_resize(&mut self.words, at + image.len().div_ceil(8), 0, "pgrc statement buffer")?;
        // SAFETY: the words just added hold at least `image.len()` bytes.
        unsafe { std::ptr::copy_nonoverlapping(image.as_ptr(), self.words.as_mut_ptr().add(at).cast::<u8>(), image.len()) };
        self.spans.push((at, image.len() as u32));
        self.bytes += image.len();
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn images_mut(&mut self) -> impl Iterator<Item = &mut [u8]> + '_ {
        let base = self.words.as_mut_ptr().cast::<u8>();
        // SAFETY: the spans are disjoint ranges of `words`, which the borrow keeps alive and unmoved.
        self.spans.iter().map(move |&(at, len)| unsafe { std::slice::from_raw_parts_mut(base.add(at * 8), len as usize) })
    }
}

pub fn full(rows: &Rows) -> bool {
    rows.len() as u64 >= bulk_rows() || rows.bytes() >= BUFFER_BYTES
}

struct Stmt {
    relid: Oid,
    cid: u32,
    subid: u32,
    bulk: bool,
    rows: Rows,
}

thread_local! {
    static STMTS: RefCell<Vec<Stmt>> = const { RefCell::new(Vec::new()) };
}

/// `handed` rows hold `tid_of` the ID the bulk lane handed them, which their part's seal moves (a `sorted_by` table):
/// they wait until [`take_indexed`]'s `place` knows the ID they landed at.
struct Indexed {
    relid: Oid,
    subid: u32,
    rows: Rows,
    tids: Vec<ItemPointerData>,
    handed: bool,
}

thread_local! {
    static INDEXED: RefCell<Vec<Indexed>> = const { RefCell::new(Vec::new()) };
}

pub fn pend_indexed(relid: Oid, subid: u32, rows: Rows, tids: Vec<ItemPointerData>, handed: bool) {
    debug_assert_eq!(rows.len(), tids.len());
    if !rows.is_empty() {
        INDEXED.with(|s| s.borrow_mut().push(Indexed { relid, subid, rows, tids, handed }));
    }
}

pub fn pend_handed(relid: Oid, subid: u32, image: &[u8], tid: ItemPointerData) -> PgResult<()> {
    INDEXED.with(|s| {
        let mut s = s.borrow_mut();
        match s.last_mut().filter(|t| (t.relid, t.subid, t.handed) == (relid, subid, true)) {
            Some(t) => {
                t.rows.push(image)?;
                t.tids.push(tid);
            }
            None => {
                let mut rows = Rows::default();
                rows.push(image)?;
                s.push(Indexed { relid, subid, rows, tids: vec![tid], handed: true });
            }
        }
        Ok(())
    })
}

pub fn take_indexed(relid: Oid, mut place: Option<&mut dyn FnMut(ItemPointerData) -> Option<ItemPointerData>>) -> PgResult<Vec<(Rows, Vec<ItemPointerData>)>> {
    INDEXED.with(|s| {
        let mut s = s.borrow_mut();
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            if s[i].relid != relid {
                i += 1;
                continue;
            }
            if !s[i].handed {
                let t = s.remove(i);
                out.push((t.rows, t.tids));
                continue;
            }
            let Some(place) = place.as_mut() else {
                i += 1;
                continue;
            };
            let t = &mut s[i];
            let (mut now, mut now_tids, mut wait, mut wait_tids) = (Rows::default(), Vec::new(), Rows::default(), Vec::new());
            for (image, &tid) in t.rows.images_mut().zip(&t.tids) {
                match place(tid) {
                    Some(at) => {
                        now.push(image)?;
                        now_tids.push(at);
                    }
                    None => {
                        wait.push(image)?;
                        wait_tids.push(tid);
                    }
                }
            }
            if !now.is_empty() {
                out.push((now, now_tids));
            }
            if wait.is_empty() {
                s.remove(i);
            } else {
                (t.rows, t.tids) = (wait, wait_tids);
                i += 1;
            }
        }
        Ok(out)
    })
}

pub fn unplaced(relid: Oid) -> bool {
    INDEXED.with(|s| s.borrow().iter().any(|t| t.relid == relid && t.handed))
}

pub fn pending_indexed() -> Option<Oid> {
    INDEXED.with(|s| s.borrow().first().map(|t| t.relid))
}

fn find(s: &[Stmt], relid: Oid, cid: u32, subid: u32) -> Option<usize> {
    s.iter().position(|t| (t.relid, t.cid, t.subid) == (relid, cid, subid))
}

pub fn switched(relid: Oid, cid: u32, subid: u32) -> bool {
    STMTS.with(|s| {
        let s = s.borrow();
        find(&s, relid, cid, subid).is_some_and(|i| s[i].bulk)
    })
}

pub fn push(relid: Oid, cid: u32, subid: u32, image: &[u8]) -> PgResult<bool> {
    STMTS.with(|s| {
        let mut s = s.borrow_mut();
        let i = find(&s, relid, cid, subid).unwrap_or_else(|| {
            s.push(Stmt { relid, cid, subid, bulk: false, rows: Rows::default() });
            s.len() - 1
        });
        s[i].rows.push(image)?;
        Ok(full(&s[i].rows))
    })
}

pub fn spill(relid: Oid, cid: u32, subid: u32) -> Rows {
    STMTS.with(|s| {
        let mut s = s.borrow_mut();
        match find(&s, relid, cid, subid) {
            Some(i) => {
                s[i].bulk = true;
                std::mem::take(&mut s[i].rows)
            }
            None => Rows::default(),
        }
    })
}

pub fn take(relid: Oid, subid: u32) -> Vec<(u32, Rows)> {
    STMTS.with(|s| {
        let mut s = s.borrow_mut();
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            if (s[i].relid, s[i].subid) == (relid, subid) {
                let t = s.remove(i);
                if !t.rows.is_empty() {
                    out.push((t.cid, t.rows));
                }
            } else {
                i += 1;
            }
        }
        out
    })
}

/// A subtransaction abort drops what it and its children buffered (they have the larger ids); the top level's
/// `TopSubTransactionId` drops everything.
pub fn discard(subid: u32) {
    STMTS.with(|s| s.borrow_mut().retain(|t| t.subid < subid));
    INDEXED.with(|s| s.borrow_mut().retain(|t| t.subid < subid));
}

pub fn reparent(subid: u32, parent: u32) {
    STMTS.with(|s| s.borrow_mut().iter_mut().filter(|t| t.subid == subid).for_each(|t| t.subid = parent));
    INDEXED.with(|s| s.borrow_mut().iter_mut().filter(|t| t.subid == subid).for_each(|t| t.subid = parent));
}

pub fn pending() -> Option<Oid> {
    STMTS.with(|s| s.borrow().iter().find(|t| !t.rows.is_empty()).map(|t| t.relid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(n: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| n.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn rows_keep_order_bytes_and_alignment() {
        let mut r = Rows::default();
        for (n, len) in [(1u8, 23usize), (2, 8), (3, 41)] {
            r.push(&image(n, len)).unwrap();
        }
        assert_eq!((r.len(), r.bytes()), (3, 72));
        let got: Vec<Vec<u8>> = r.images_mut().map(|i| i.to_vec()).collect();
        assert_eq!(got, vec![image(1, 23), image(2, 8), image(3, 41)]);
        assert!(r.images_mut().all(|i| i.as_ptr() as usize % 8 == 0));
        r.images_mut().next().unwrap()[0] = 0xff;
        assert_eq!(r.images_mut().next().unwrap()[0], 0xff, "images are written in place");
    }

    #[test]
    fn the_buffer_is_full_at_the_byte_cap() {
        discard(1);
        let row = image(0, 24);
        let n = BUFFER_BYTES.div_ceil(24);
        assert!(!(1..n).any(|_| push(10, 3, 1, &row).unwrap()));
        assert!(!switched(10, 3, 1));
        assert!(push(10, 3, 1, &row).unwrap(), "the row that reaches the cap fills it");
        let spilled = spill(10, 3, 1);
        assert_eq!(spilled.len(), n);
        assert!(switched(10, 3, 1) && !switched(10, 4, 1), "a new command id is a new statement");
        assert!(take(10, 1).is_empty(), "a switched statement has nothing left for the segment");
        assert!(!switched(10, 3, 1), "statement end forgets it");

        let wide = image(0, 1 << 20);
        assert!(!(1..BUFFER_BYTES >> 20).any(|_| push(11, 5, 1, &wide).unwrap()));
        assert!(push(11, 5, 1, &wide).unwrap(), "wide rows reach the cap in few rows");
        discard(1);
    }

    #[test]
    fn statement_end_takes_its_subtransaction_commits_reparent_and_aborts_drop_theirs_and_their_childrens() {
        discard(1);
        push(20, 7, 1, &image(1, 16)).unwrap();
        push(20, 8, 1, &image(2, 16)).unwrap();
        push(21, 8, 1, &image(3, 16)).unwrap();
        push(20, 9, 3, &image(4, 16)).unwrap();
        push(20, 9, 4, &image(5, 16)).unwrap();
        push(20, 6, 2, &image(6, 16)).unwrap();
        reparent(2, 1);
        discard(3);
        assert_eq!(pending(), Some(20));
        let mut got = take(20, 1);
        assert_eq!(got.iter().map(|(c, r)| (*c, r.len())).collect::<Vec<_>>(), vec![(7, 1), (8, 1), (6, 1)], "a committed subtransaction's rows are its parent's");
        assert_eq!(got[1].1.images_mut().next().unwrap().to_vec(), image(2, 16));
        assert_eq!(pending(), Some(21), "another relation waits for its own statement end");
        assert!(take(21, 2).is_empty(), "another subtransaction's statement end takes nothing");
        discard(1);
        assert_eq!(pending(), None);
    }

    fn tid(off: u16) -> ItemPointerData {
        let mut t = ItemPointerData::invalid();
        types_tuple::ItemPointerSet(&mut t, 0, off);
        t
    }

    fn rows(images: &[(u8, usize)]) -> Rows {
        let mut r = Rows::default();
        images.iter().for_each(|&(n, len)| r.push(&image(n, len)).unwrap());
        r
    }

    #[test]
    fn indexed_rows_wait_per_relation_in_order_and_follow_their_subtransaction() {
        discard(1);
        pend_indexed(30, 1, rows(&[(1, 16), (2, 24)]), vec![tid(1), tid(2)], false);
        pend_indexed(31, 1, rows(&[(3, 16)]), vec![tid(3)], false);
        pend_indexed(30, 2, rows(&[(4, 16)]), vec![tid(4)], false);
        pend_indexed(30, 3, rows(&[(5, 16)]), vec![tid(5)], false);
        pend_indexed(30, 1, Rows::default(), Vec::new(), false);
        reparent(2, 1);
        discard(3);
        assert_eq!(pending_indexed(), Some(30));
        let mut got = take_indexed(30, None).unwrap();
        assert_eq!(got.iter().map(|(r, t)| (r.len(), t.clone())).collect::<Vec<_>>(), vec![(2, vec![tid(1), tid(2)]), (1, vec![tid(4)])], "insertion order; an aborted subtransaction's rows are gone; empty batches are not kept");
        assert_eq!(got[0].0.images_mut().nth(1).unwrap().to_vec(), image(2, 24));
        assert!(take_indexed(30, None).unwrap().is_empty(), "taken rows are forgotten");
        assert_eq!(pending_indexed(), Some(31), "another relation's rows still wait");
        discard(1);
        assert_eq!(pending_indexed(), None, "the top-level abort drops them");
    }

    #[test]
    fn handed_rows_go_at_their_placed_ids_and_the_unplaced_keep_waiting() {
        discard(1);
        pend_indexed(40, 1, rows(&[(1, 16)]), vec![tid(1)], false);
        pend_indexed(40, 1, rows(&[(2, 16), (3, 24)]), vec![tid(2), tid(3)], true);
        pend_handed(40, 1, &image(4, 16), tid(4)).unwrap();
        pend_handed(40, 2, &image(5, 16), tid(5)).unwrap();
        assert!(unplaced(40));
        let got = take_indexed(40, None).unwrap();
        assert_eq!(got.iter().map(|(r, t)| (r.len(), t.clone())).collect::<Vec<_>>(), vec![(1, vec![tid(1)])], "no place: only the rows with their IDs");
        let mut place = |t: ItemPointerData| [2u16, 4].contains(&t.ip_posid).then(|| tid(t.ip_posid + 100));
        let mut got = take_indexed(40, Some(&mut place)).unwrap();
        assert_eq!(got.iter().map(|(r, t)| (r.len(), t.clone())).collect::<Vec<_>>(), vec![(2, vec![tid(102), tid(104)])], "a handed row joined its subtransaction's last batch");
        assert_eq!(got[0].0.images_mut().map(|i| i.to_vec()).collect::<Vec<_>>(), vec![image(2, 16), image(4, 16)]);
        assert!(unplaced(40));
        discard(2);
        let mut all = |t: ItemPointerData| Some(tid(t.ip_posid + 100));
        let got = take_indexed(40, Some(&mut all)).unwrap();
        assert_eq!(got.iter().map(|(r, t)| (r.len(), t.clone())).collect::<Vec<_>>(), vec![(1, vec![tid(103)])], "the aborted subtransaction's row is gone");
        assert!(!unplaced(40));
        assert_eq!(pending_indexed(), None);
    }
}
