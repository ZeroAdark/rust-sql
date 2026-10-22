//! A bitmap heap scan of a pgrc relation.
//! SSI: the scan takes the relation-grain predicate lock a pgrc seq scan takes (`filter::predicate_lock`).
//! Gravity's bitmap source reads it too, through tableam; an exact page is never rechecked (D4674).

use std::sync::Arc;

use mcx::Mcx;
use pgrc_am::catalog::Generation;
use pgrc_am::scan::BitmapSource;
use pgrc_format::rowid::{id_of, IDS_PER_BLOCK, ROWID_FIRST};
use tableam_vocab::Snapshot;
use types_error::PgResult;
use types_rel::Relation;
use types_slot::SlotData;

use crate::ixfetch::{self, Hit, Kind, PgrcIndexFetch};

pub struct BitmapScan<'mcx> {
    mcx: Mcx<'mcx>,
    fetch: PgrcIndexFetch<'mcx>,
    snapshot: Snapshot<'mcx>,
    kind: Kind,
    staged: Vec<Hit>,
    gen: Option<Arc<Generation>>,
    cursor: u32,
}

pub fn begin<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, snapshot: Snapshot<'mcx>) -> PgResult<BitmapScan<'mcx>> {
    let kind = ixfetch::kind_of(&snapshot)?;
    crate::filter::predicate_lock(rel, &snapshot)?;
    Ok(BitmapScan { mcx, fetch: ixfetch::begin(rel), snapshot, kind, staged: Vec::new(), gen: None, cursor: 0 })
}

pub(crate) fn page_ids(block: u32, offsets: Option<&[u16]>) -> impl Iterator<Item = u64> + '_ {
    let lossy = offsets.is_none().then_some(1..=IDS_PER_BLOCK as u16).into_iter().flatten();
    let exact = offsets.into_iter().flatten().copied();
    lossy.chain(exact).filter_map(move |off| id_of(block, off)).filter(|&id| id >= ROWID_FIRST)
}

fn decode_key(h: &Hit) -> (u8, usize, u32, u32) {
    match *h {
        Hit::Segment { .. } => (0, 0, 0, 0),
        Hit::Part { part, granule, row, .. } => (1, part, granule, row),
    }
}

pub(crate) fn decode_order(staged: &mut [Hit]) {
    if staged.windows(2).any(|w| decode_key(&w[0]) > decode_key(&w[1])) {
        staged.sort_by_key(decode_key);
    }
}

impl BitmapSource for BitmapScan<'_> {
    fn stage(&mut self, block: u32, offsets: Option<&[u16]>) -> PgResult<u32> {
        // once per page, as heap's bitmap_next_block checks
        if init_small::globals::InterruptPending() {
            postgres_seams::check_for_interrupts::call()?;
        }
        self.staged.clear();
        self.cursor = 0;
        let mcx = self.mcx;
        for id in page_ids(block, offsets) {
            if let Some(h) = self.fetch.stage_id(mcx, id, self.kind, &self.snapshot)? {
                self.staged.push(h);
            }
        }
        // every hit of the page was judged in the fetch state's current view (the memo is per snapshot and command)
        self.gen = self.fetch.view();
        decode_order(&mut self.staged);
        Ok(self.staged.len() as u32)
    }

    fn store<'s>(&mut self, mcx: Mcx<'s>, i: u32, slot: &mut SlotData<'s>) -> PgResult<()> {
        let hit = self.staged[i as usize];
        self.fetch.store_hit(self.mcx, mcx, self.gen.as_ref(), &hit, &self.snapshot, slot)?;
        self.cursor = i + 1;
        Ok(())
    }

    fn next<'s>(&mut self, mcx: Mcx<'s>, slot: &mut SlotData<'s>) -> PgResult<bool> {
        if self.cursor as usize >= self.staged.len() {
            return Ok(false);
        }
        self.store(mcx, self.cursor, slot)?;
        Ok(true)
    }

    fn set_needed(&mut self, needed: &[bool]) {
        self.fetch.set_needed_attrs(Some(needed.into()));
    }

    fn reset(&mut self) {
        self.staged.clear();
        self.gen = None;
        self.cursor = 0;
    }

    fn end(self: Box<Self>) -> PgResult<()> {
        let this = *self;
        this.fetch.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_tuple::{ItemPointerData, ItemPointerSet};

    fn seg(id: u64) -> Hit {
        let mut tid = ItemPointerData::default();
        ItemPointerSet(&mut tid, 3, (id % 291) as u16 + 1);
        Hit::Segment { relid: 16000, tid, id }
    }

    fn part(part: usize, granule: u32, row: u32, id: u64) -> Hit {
        Hit::Part { part, granule, row, id }
    }

    #[test]
    fn a_page_names_the_ids_of_its_virtual_block() {
        assert_eq!(page_ids(2, Some(&[1, 5, 291])).collect::<Vec<_>>(), vec![582, 586, 872]);
        assert_eq!(page_ids(2, Some(&[0, 292, 7])).collect::<Vec<_>>(), vec![588]);
        let lossy: Vec<u64> = page_ids(3, None).collect();
        assert_eq!(lossy.len(), 291);
        assert_eq!((lossy[0], lossy[290]), (873, 1163));
        assert!(lossy.windows(2).all(|w| w[1] == w[0] + 1));
        assert_eq!(page_ids(0, None).count(), 0);
        assert_eq!(page_ids(0, Some(&[1, 291])).count(), 0);
        assert_eq!(page_ids(1, Some(&[1])).collect::<Vec<_>>(), vec![ROWID_FIRST]);
        for id in page_ids(4000, None) {
            assert_eq!(pgrc_format::rowid::tid_of(id).0, 4000);
        }
    }

    #[test]
    fn a_page_decodes_in_physical_order() {
        let mut page = vec![part(1, 7, 3, 600), seg(601), part(0, 2, 9, 602), part(1, 0, 4, 603), seg(604), part(1, 7, 1, 605)];
        decode_order(&mut page);
        let ids: Vec<u64> = page.iter().map(Hit::id).collect();
        assert_eq!(ids, vec![601, 604, 602, 603, 605, 600]);
        let mut sorted = vec![part(0, 0, 0, 900), part(0, 0, 1, 901), part(0, 1, 0, 902)];
        let before = sorted.clone();
        decode_order(&mut sorted);
        assert_eq!(sorted, before);
        let mut empty: Vec<Hit> = Vec::new();
        decode_order(&mut empty);
        assert!(empty.is_empty());
    }
}
