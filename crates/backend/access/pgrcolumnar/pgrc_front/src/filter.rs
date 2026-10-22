//! The deletion filter: the tombstones a scan's snapshot sees, keyed to what the scan reads.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use mcx::{Mcx, PgVec};
use pgrc_am::catalog::Generation;
use pgrc_am::scan::DeletionFilter;
use pgrc_format::dml::Tombstone;
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::manifest::{SegState, SegmentRecord};
use tableam_vocab::{Snapshot, SO_ALLOW_PAGEMODE, SO_TYPE_SEQSCAN};
use types_core::{ForkNumber, InvalidTransactionId, TransactionId};
use types_error::{PgError, PgResult};
use types_rel::Relation;
use types_scan::sdir::ScanDirection;
use types_snapshot::{SnapshotData, SnapshotType};

pub const LIST_PROMOTE_AT: usize = 128;
const DV_GRANULE_WORDS: usize = GRANULE_ROWS as usize / 64;

enum Granule {
    List(Vec<u16>),
    Bits(Box<[u64; DV_GRANULE_WORDS]>),
}

impl Granule {
    fn insert(&mut self, row: u16) {
        match self {
            Granule::List(l) => {
                if let Err(at) = l.binary_search(&row) {
                    l.insert(at, row);
                }
                if l.len() > LIST_PROMOTE_AT {
                    let mut words = Box::new([0u64; DV_GRANULE_WORDS]);
                    l.iter().for_each(|&r| words[r as usize / 64] |= 1 << (r % 64));
                    *self = Granule::Bits(words);
                }
            }
            Granule::Bits(w) => w[row as usize / 64] |= 1 << (row % 64),
        }
    }

    fn or_into(&self, mask: &mut [u64; DV_GRANULE_WORDS]) {
        match self {
            Granule::List(l) => l.iter().for_each(|&r| mask[r as usize / 64] |= 1 << (r % 64)),
            Granule::Bits(w) => mask.iter_mut().zip(w.iter()).for_each(|(d, s)| *d |= s),
        }
    }
}

#[derive(Default)]
pub struct TombFilter {
    parts: Vec<BTreeMap<u32, Granule>>,
    segments: HashMap<u16, Vec<u32>>,
    /// SnapshotDirty only: the rows another transaction's tombstone is deleting, still in progress. They stay
    /// visible and report the deleter as the snapshot's xmax, as heap's dirty read reports an in-progress xmax.
    busy_parts: HashMap<(usize, u32, u32), TransactionId>,
    busy_segments: HashMap<u16, Vec<(u32, TransactionId)>>,
}

impl TombFilter {
    pub fn with_parts(n: usize) -> TombFilter {
        TombFilter { parts: (0..n).map(|_| BTreeMap::new()).collect(), ..TombFilter::default() }
    }

    pub fn add_part_row(&mut self, part: usize, granule: u32, row: u32) {
        debug_assert!(row < GRANULE_ROWS);
        self.parts[part].entry(granule).or_insert_with(|| Granule::List(Vec::new())).insert(row as u16);
    }

    pub fn add_segment_row(&mut self, slot: u16, pos: u32) {
        self.segments.entry(slot).or_default().push(pos);
    }

    fn add_busy_part_row(&mut self, part: usize, granule: u32, row: u32, xmax: TransactionId) {
        self.busy_parts.insert((part, granule, row), xmax);
    }

    fn add_busy_segment_row(&mut self, slot: u16, pos: u32, xmax: TransactionId) {
        self.busy_segments.entry(slot).or_default().push((pos, xmax));
    }

    pub fn finished(self) -> TombFilter {
        self.finish()
    }

    fn finish(mut self) -> TombFilter {
        for v in self.segments.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        for v in self.busy_segments.values_mut() {
            v.sort_unstable();
            v.dedup_by_key(|b| b.0);
        }
        self
    }
}

impl DeletionFilter for TombFilter {
    fn granule_into(&self, part: usize, g: u32, mask: &mut Option<Box<[u64; DV_GRANULE_WORDS]>>) {
        if let Some(d) = self.parts.get(part).and_then(|p| p.get(&g)) {
            d.or_into(mask.get_or_insert_with(|| Box::new([0; DV_GRANULE_WORDS])));
        }
    }

    fn segment_positions(&self, slot: u16) -> &[u32] {
        self.segments.get(&slot).map_or(&[], |v| v)
    }

    fn busy_part(&self, part: usize, g: u32, row: u32) -> Option<TransactionId> {
        if self.busy_parts.is_empty() {
            return None;
        }
        self.busy_parts.get(&(part, g, row)).copied()
    }

    fn busy_segment(&self, slot: u16) -> &[(u32, TransactionId)] {
        self.busy_segments.get(&slot).map_or(&[], |v| v)
    }
}

/// SSI at relation grain on the parent, as heap_beginscan's seqscan lock (the companions' own scans lock only the
/// companions). Every scan takes it, a table without tombstones or without a publish included: it is read all the same.
pub fn predicate_lock(rel: &Relation<'_>, snap: &Snapshot<'_>) -> PgResult<()> {
    match snap.as_deref().filter(|s| s.snapshot_type != SnapshotType::SNAPSHOT_ANY && xact::IsolationIsSerializable()) {
        Some(s) => predicate_seams::predicate_lock_relation::call(rel, s),
        None => Ok(()),
    }
}

/// `CheckForSerializableConflictIn(parent)` at relation grain, before a tombstone insert and before an INSERT's row
/// (heap_insert's check: the segment's own heap insert checks only the segment, which a scan of a sealed table never
/// locked).
pub fn conflict_in(rel: &Relation<'_>) -> PgResult<()> {
    if !xact::IsolationIsSerializable() {
        return Ok(());
    }
    predicate_seams::check_for_serializable_conflict_in::call(rel, None, types_core::InvalidBlockNumber)
}

pub fn build<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, gen: Option<&Arc<Generation>>, newer: &[SegmentRecord], snap: &Snapshot<'mcx>) -> PgResult<Option<TombFilter>> {
    if snap.as_ref().is_none_or(|s| s.snapshot_type == SnapshotType::SNAPSHOT_ANY) {
        return Ok(None);
    }
    predicate_lock(rel, snap)?;
    injection_point::injection_point("pgrc-front-scan-start")?;
    if gen.is_none() && newer.is_empty() {
        return Ok(None);
    }
    let slots = crate::tombs::read_slots(&crate::tombs::state(gen.map(|g| &**g))?, &crate::tombs::newest_state(rel)?);
    let heaps = crate::tombs::open_read(mcx, rel.rd_id, &slots)?;
    let mut out: Option<(TombFilter, Vec<Option<Arc<pgrc_am::catalog::RowMap>>>)> = None;
    let res = (|| {
        for h in &heaps {
            if bufmgr::RelationGetNumberOfBlocksInFork(&h.rel, ForkNumber::MAIN_FORKNUM)? == 0 {
                continue;
            }
            let nparts = gen.map_or(0, |g| g.parts.len());
            if out.is_none() {
                out = Some((TombFilter::with_parts(nparts), ::mcx::stdvec::try_filled(nparts, None, "pgrc tombstone filter")?));
            }
            if let Some((f, maps)) = out.as_mut() {
                scan(mcx, rel, &h.rel, gen, newer, snap, f, maps)?;
            }
        }
        Ok(())
    })();
    crate::tombs::close_all(heaps)?;
    res.map(|()| out.map(|(f, _)| f.finish()))
}

#[allow(clippy::too_many_arguments)]
fn scan<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tomb: &Relation<'mcx>,
    gen: Option<&Arc<Generation>>,
    newer: &[SegmentRecord],
    snap: &Snapshot<'mcx>,
    f: &mut TombFilter,
    maps: &mut [Option<Arc<pgrc_am::catalog::RowMap>>],
) -> PgResult<()> {
    let view = gen.map_or(&[][..], |g| &g.segments[..]);
    let live: HashMap<u32, u16> = view.iter().chain(newer).filter(|r| r.state != SegState::Sealed).map(|r| (r.part_no, r.slot)).collect();
    let dir = pgrc_am::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let ext = pgrc_am::rowid_alloc::extents(&dir)?;
    // SnapshotDirty: the tombstone heaps read under a snapshot of their own (a dirty read writes each tuple's xids into
    // its snapshot, and the reader's out-fields are the rows' to set); a tombstone whose inserter is still in progress
    // keeps its row visible, busy
    let dirty = snap.as_deref().is_some_and(|s| s.snapshot_type == SnapshotType::SNAPSHOT_DIRTY);
    let tsnap: Snapshot<'mcx> = match dirty {
        true => Some(Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY))),
        false => snap.clone(),
    };
    let mut hs = heapam::heap_beginscan(mcx, tomb, tsnap.clone(), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE)?;
    let out = (|| {
        while let Some(tup) = heapam::heap_getnext(&mut hs, ScanDirection::ForwardScanDirection)? {
            let busy = match tsnap.as_deref() {
                Some(s) if dirty && s.dirty_xmin.get() != InvalidTransactionId => Some(s.dirty_xmin.get()),
                _ => None,
            };
            let (mut v, mut n) = ([datum::Datum::null(); 2], [false; 2]);
            types_tuple::heap_deform_tuple(tup, &tomb.rd_att, &mut v, &mut n);
            let t = Tombstone::from_datums([v[0].as_i64(), v[1].as_i64()]).map_err(|e| corrupt(format!("{e}")))?;
            if t.is_lock() {
                continue;
            }
            // a live segment's row (its extent names the segment), else the part of the view whose ranges hold it
            if let Some((part_no, local)) = ext.seg_of(t.rowident) {
                if let Some(&slot) = live.get(&part_no) {
                    match busy {
                        Some(x) => f.add_busy_segment_row(slot, local, x),
                        None => f.add_segment_row(slot, local),
                    }
                    continue;
                }
            }
            let Some(gen) = gen else { continue };
            let mut map_of = |i: usize| -> PgResult<Arc<pgrc_am::catalog::RowMap>> {
                Ok(match &maps[i] {
                    Some(m) => Arc::clone(m),
                    None => Arc::clone(maps[i].insert(gen.row_map(i, &dir)?)),
                })
            };
            let Some((i, ord, _)) = gen.locate(t.rowident, &mut map_of)? else { continue };
            let rec = &gen.manifest.parts[i];
            let grain = match rec.granule_rows {
                0 => GRANULE_ROWS,
                g => g,
            };
            match busy {
                Some(x) => f.add_busy_part_row(i, ord / grain, ord % grain, x),
                None => f.add_part_row(i, ord / grain, ord % grain),
            }
        }
        Ok(())
    })();
    heapam::heap_endscan(hs)?;
    out
}

fn corrupt(what: String) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar: tombstone {what}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_matches_a_set_oracle_and_segments_sort() {
        let mut f = TombFilter::with_parts(3);
        let mut oracle: BTreeMap<(usize, u32), std::collections::BTreeSet<u32>> = BTreeMap::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for i in 0..5000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let (part, g) = ((x % 3) as usize, (x >> 8) as u32 % 4);
            let row = if (part, g) == (0, 0) { (x >> 20) as u32 % GRANULE_ROWS } else { (x >> 20) as u32 % 16 * 97 };
            f.add_part_row(part, g, row);
            oracle.entry((part, g)).or_default().insert(row);
            if i % 7 == 0 {
                f.add_segment_row((i % 2) as u16, 5000 - i);
                f.add_segment_row((i % 2) as u16, 5000 - i);
            }
        }
        assert!(matches!(f.parts[0].get(&0), Some(Granule::Bits(_))));
        assert!(f.parts[1].values().all(|g| matches!(g, Granule::List(_))));
        for part in 0..3 {
            for g in 0..5 {
                let mut mask = None;
                f.granule_into(part, g, &mut mask);
                let want = oracle.get(&(part, g));
                assert_eq!(mask.is_some(), want.is_some(), "part {part} granule {g}");
                if let (Some(m), Some(w)) = (mask, want) {
                    let got: Vec<u32> = (0..GRANULE_ROWS).filter(|&r| m[r as usize / 64] >> (r % 64) & 1 == 1).collect();
                    assert_eq!(got, w.iter().copied().collect::<Vec<_>>());
                }
            }
        }
        let mut dv = Some(Box::new([0u64; DV_GRANULE_WORDS]));
        dv.as_mut().unwrap()[0] = 1 << 5;
        f.granule_into(7, 0, &mut dv);
        assert_eq!(dv.unwrap()[0], 1 << 5, "an unknown part adds nothing");
        let f = f.finish();
        let s = f.segment_positions(0);
        assert!(s.windows(2).all(|w| w[0] < w[1]) && s.len() == 358);
        assert!(f.segment_positions(9).is_empty());
    }

    #[test]
    fn busy_rows_stay_visible_and_name_their_deleter() {
        let mut f = TombFilter::with_parts(2);
        f.add_part_row(0, 0, 5);
        f.add_busy_part_row(0, 0, 6, 900);
        f.add_busy_part_row(1, 3, 7, 901);
        f.add_segment_row(4, 10);
        f.add_busy_segment_row(4, 30, 903);
        f.add_busy_segment_row(4, 20, 902);
        f.add_busy_segment_row(4, 20, 902);
        let f = f.finish();
        let mut mask = None;
        f.granule_into(0, 0, &mut mask);
        assert_eq!(mask.unwrap()[0], 1 << 5);
        let mut mask = None;
        f.granule_into(1, 3, &mut mask);
        assert!(mask.is_none());
        assert_eq!(f.busy_part(0, 0, 6), Some(900));
        assert_eq!(f.busy_part(1, 3, 7), Some(901));
        assert_eq!(f.busy_part(0, 0, 5), None);
        assert_eq!(f.segment_positions(4), &[10]);
        assert_eq!(f.busy_segment(4), &[(20, 902), (30, 903)]);
        assert!(f.busy_segment(5).is_empty());
    }
}
