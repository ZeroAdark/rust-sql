//! ANALYZE's two-pass sampler over a pgrcolumnar table.

use mcx::Mcx;
use pgrc_am::scan::{PgrcScanDescData, SEGMENT_POSITION};
use types_error::PgResult;
use types_slot::SlotData;

use crate::segscan::SegScan;

pub fn total_rows<'mcx>(mcx: Mcx<'mcx>, c: &mut PgrcScanDescData<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<u64> {
    under_snapshot(mcx, c)?;
    let parts = c.live_part_rows()?;
    let mut segments = 0u64;
    while next_segment_row(mcx, c, slot)? {
        segments += 1;
    }
    c.set_sample_segments(segments);
    slot.base_mut().mark_empty();
    Ok(parts + segments)
}

pub fn fetch<'mcx>(mcx: Mcx<'mcx>, c: &mut PgrcScanDescData<'mcx>, pos: u64, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    if pos & SEGMENT_POSITION == 0 {
        return c.fetch_sample(pos, slot);
    }
    let k = pos & !SEGMENT_POSITION;
    loop {
        let at = c.sample_segment_next();
        // a row already passed (the positions ascend) or one past what the count pass read is no live row
        if at > k || !next_segment_row(mcx, c, slot)? {
            slot.base_mut().mark_empty();
            return Ok(false);
        }
        c.sample_segment_step();
        if at == k {
            return Ok(true);
        }
    }
}

/// ANALYZE begins its scan with no snapshot (the heap sampler judges each tuple against OldestXmin), which leaves a
/// pgrcolumnar scan with no tombstone filter and its segment heaps read whole: every row ever written would count as
/// live. The sampler reads under the statement's active snapshot (else the transaction's) instead: the rows
/// `getnextslot` returns to that statement, the snapshot's tombstones applied to the parts and to the segments.
fn under_snapshot<'mcx>(mcx: Mcx<'mcx>, c: &mut PgrcScanDescData<'mcx>) -> PgResult<()> {
    if c.rs_base.rs_snapshot.is_some() {
        return Ok(());
    }
    let snap = if snapmgr::ActiveSnapshotSet() { snapmgr::GetActiveSnapshot() } else { snapmgr::GetTransactionSnapshot()? };
    c.rs_base.rs_snapshot = Some(snap);
    let gen = c.generation().cloned();
    let newer = crate::ring::newer_segments(&c.rs_base.rs_rd, gen.as_deref())?;
    let filter = crate::filter::build(mcx, &c.rs_base.rs_rd, gen.as_ref(), &newer, &c.rs_base.rs_snapshot)?;
    if let Some(f) = filter {
        c.filter = Some(Box::new(f));
    }
    Ok(())
}

fn next_segment_row<'mcx>(mcx: Mcx<'mcx>, c: &mut PgrcScanDescData<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    loop {
        if let Some(seg) = c.segment.as_mut() {
            if seg.next(slot)? {
                return Ok(true);
            }
            if let Some(done) = c.segment.take() {
                done.end()?;
            }
        }
        let Some((segslot, relid, part_no)) = c.claim_segment() else { return Ok(false) };
        let snapshot = c.rs_base.rs_snapshot.clone();
        let parent = c.rs_base.rs_rd.rd_id;
        let deleted = match c.filter.as_ref() {
            Some(f) => ::mcx::stdvec::try_to_vec(f.segment_positions(segslot), "pgrc segment sample")?,
            None => Vec::new(),
        };
        c.segment = Some(Box::new(SegScan::begin(mcx, &c.rs_base.rs_rd, relid, segslot, part_no, snapshot, parent, deleted)?));
    }
}
