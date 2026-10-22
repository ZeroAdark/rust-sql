//! Reclaim below the horizon: sealed and unnamed segments, older generations' files.

use mcx::Mcx;
use pgrc_format::manifest::{segments, SegState, SegmentRecord};
use types_core::ForkNumber;
use types_error::PgResult;
use types_rel::{AccessExclusiveLock, NoLock, Relation};

use crate::ring;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimed {
    pub segments: u32,
    pub files: u32,
}

pub fn settle(rel: &Relation<'_>) -> PgResult<bool> {
    if !pgrc_am::ingest::unsettled(rel)? {
        return Ok(false);
    }
    pgrc_am::ingest::publish_ext_maint(rel, &mut |_, _| Ok(()))?;
    Ok(true)
}

/// A segment is freed only under a conditional AccessExclusive, which a standby replays as its conflict with the
/// readers that hold the segment; readers whose snapshot predates its seal conflict at the seal's horizon.
pub fn reclaim<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Reclaimed> {
    let horizon = procarray::GetOldestNonRemovableTransactionId(rel)?;
    let Some(keep) = pgrc_am::ingest::keep_generation(rel, horizon)? else { return Ok(Reclaimed::default()) };
    // `keep` stays put while the horizon is held, so its Sealed records outlive their own reclaim: once one is freed
    // its slot is reused (same companion relid, a new part_no) and holds new committed rows. Free only a record the
    // newest generation still lists Sealed, rechecked under the lock (a reclaim or activation of the slot in between
    // needed the lock we now hold).
    let mut freed = Vec::new();
    for r in keep.segments.iter().filter(|r| r.state == SegState::Sealed) {
        if still_sealed(rel, r)? && lmgr::ConditionalLockRelationOid(r.relid, AccessExclusiveLock)? {
            if still_sealed(rel, r)? {
                freed.push(*r);
            } else {
                lmgr::UnlockRelationOid(r.relid, AccessExclusiveLock)?;
            }
        }
    }
    if !freed.is_empty() {
        // the conflict first, at the seal's horizon, so a standby resolves it before it replays the reset
        let (xid, keep_from) = pgrc_am::ingest::seal_horizon(rel, &keep, &freed)?;
        pgrc_am::ingest::log_segment_conflict(rel, xid, keep_from)?;
        pgrc_am::ingest::publish_ext_maint(rel, &mut |base, b| {
            let mut recs = base.map(segments).transpose()?.unwrap_or_default();
            recs.retain(|r| !freed.iter().any(|f| f.slot == r.slot && f.part_no == r.part_no && r.state == SegState::Sealed));
            b.ext(ring::with_segments(base.map_or(&[][..], |m| &m.ext), &recs)?);
            Ok(())
        })?;
        for r in &freed {
            pgrc_front_seams::reset_segment::call(mcx, r.relid)?;
        }
    }
    let named: Vec<u16> = ring::newest(rel)?.map(|g| g.segments.iter().map(|r| r.slot).collect()).unwrap_or_default();
    for slot in 0.. {
        let Some(relid) = pgrc_front_seams::segment_relid::call(rel.rd_id, slot)? else { break };
        if named.contains(&slot) || !lmgr::ConditionalLockRelationOid(relid, AccessExclusiveLock)? {
            continue;
        }
        let seg = table::table_open(mcx, relid, NoLock)?;
        let n = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
        seg.close(NoLock)?;
        if n? > 0 {
            pgrc_front_seams::reset_segment::call(mcx, relid)?;
        }
    }
    let serialized = crate::maint::maint_held(rel.rd_id);
    let files = pgrc_am::ingest::reclaim_below(rel, &keep, serialized, &mut || {
        let horizon = procarray::GetOldestNonRemovableTransactionId(rel)?;
        pgrc_am::ingest::keep_generation(rel, horizon)
    })? as u32;
    Ok(Reclaimed { segments: freed.len() as u32, files })
}

fn still_sealed(rel: &Relation<'_>, r: &SegmentRecord) -> PgResult<bool> {
    Ok(ring::newest(rel)?.is_some_and(|g| listed_sealed(ring::records(&g), r)))
}

fn listed_sealed(newest: &[SegmentRecord], r: &SegmentRecord) -> bool {
    newest.iter().any(|n| n.state == SegState::Sealed && n.slot == r.slot && n.part_no == r.part_no && n.relid == r.relid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(slot: u16, part_no: u32, state: SegState) -> SegmentRecord {
        SegmentRecord { slot, part_no, relid: 500 + slot as u32, state, fxid: 7 }
    }

    #[test]
    fn a_kept_sealed_record_is_freed_only_while_the_newest_generation_still_lists_it_sealed() {
        let kept = seg(2, 10, SegState::Sealed);
        assert!(listed_sealed(&[seg(0, 30, SegState::Active), kept], &kept));
        assert!(!listed_sealed(&[seg(0, 30, SegState::Active)], &kept), "already freed: the slot is free");
        assert!(!listed_sealed(&[seg(2, 40, SegState::Active)], &kept), "the slot was reused: its rows are new");
        assert!(!listed_sealed(&[seg(2, 40, SegState::Sealing)], &kept), "the reused slot is draining");
        assert!(!listed_sealed(&[seg(2, 40, SegState::Sealed)], &kept), "a later part of the reused slot is not keep's");
    }
}
