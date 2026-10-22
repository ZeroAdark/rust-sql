//! The ring: the newest generation's segment records name the active and sealing companions.

use std::sync::Arc;

use mcx::Mcx;
use pgrc_am::catalog::{self, Generation, Lookup, RelSlot};
use pgrc_am::probe::SnapshotCommitCheck;
use pgrc_format::manifest::{encode_segments, ext_records, put_ext_record, segments, SegState, SegmentRecord, EXT_SEGMENTS};
use types_core::{ForkNumber, Oid, BLCKSZ};
use types_storage::bufpage::SizeOfPageHeaderData;
use types_error::PgResult;
use types_rel::{AccessShareLock, NoLock, Relation, RowExclusiveLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveSeg {
    pub slot: u16,
    pub part_no: u32,
    pub relid: Oid,
}

fn rel_slot(rel: &Relation<'_>) -> RelSlot {
    let l = rel.rd_locator.get();
    (l.spcOid, l.dbOid, l.relNumber as u64)
}

pub fn newest(rel: &Relation<'_>) -> PgResult<Option<Arc<Generation>>> {
    let check = SnapshotCommitCheck::new(None);
    let found = catalog::visible(rel_slot(rel), &check)?;
    check.take_error()?;
    Ok(match found {
        Lookup::Gen(g) => Some(g),
        Lookup::Empty | Lookup::Unknown => None,
    })
}

pub fn records(gen: &Generation) -> &[SegmentRecord] {
    &gen.segments
}

fn active_of(recs: &[SegmentRecord]) -> Option<ActiveSeg> {
    recs.iter().find(|r| r.state == SegState::Active).map(|r| ActiveSeg { slot: r.slot, part_no: r.part_no, relid: r.relid })
}

pub fn find_active(rel: &Relation<'_>) -> PgResult<Option<ActiveSeg>> {
    Ok(newest(rel)?.and_then(|g| active_of(records(&g))))
}

/// The live segments of the newest generation that `view` (the generation the scan's snapshot sees) does not list. A
/// switch published after the snapshot sends the transaction's own inserts (UPDATE's new versions included) to a
/// segment only newer generations name, and its RowExclusiveLock on it (held to transaction end) keeps that segment
/// from being drained, so it is live in the newest generation. Heap MVCC shows the scan only its own rows there: any
/// other row was inserted after the switch committed, so after the snapshot (the price of reading them is heap's: a
/// heap scan reads the rows inserted after its snapshot too). No lock test picks the transaction's own segments: a
/// parallel worker holds none of its leader's locks.
pub fn newer_segments(rel: &Relation<'_>, view: Option<&Generation>) -> PgResult<Vec<SegmentRecord>> {
    let Some(newest) = newest(rel)? else { return Ok(Vec::new()) };
    // a composed own view has the newest own generation's number, not its segments
    if view.is_some_and(|v| v.gen == newest.gen && !v.composed()) {
        return Ok(Vec::new());
    }
    Ok(unlisted(view.map_or(&[][..], records), records(&newest)))
}

pub(crate) fn head_is(rel: &Relation<'_>, gen: u64) -> bool {
    catalog::head(rel_slot(rel)) == Some(gen)
}

fn unlisted(view: &[SegmentRecord], newest: &[SegmentRecord]) -> Vec<SegmentRecord> {
    let listed = |r: &SegmentRecord| view.iter().any(|v| v.state != SegState::Sealed && v.relid == r.relid && v.part_no == r.part_no);
    newest.iter().filter(|r| r.state != SegState::Sealed && !listed(r)).copied().collect()
}

/// Lock-then-recheck: a stale inserter wrote nothing under its lock, so it releases it and retries.
pub fn active(rel: &Relation<'_>) -> PgResult<Option<ActiveSeg>> {
    lock_active(&mut || find_active(rel), &mut |r| lmgr::LockRelationOid(r, RowExclusiveLock), &mut |r| lmgr::UnlockRelationOid(r, RowExclusiveLock))
}

fn lock_active(find: &mut dyn FnMut() -> PgResult<Option<ActiveSeg>>, lock: &mut dyn FnMut(Oid) -> PgResult<()>, unlock: &mut dyn FnMut(Oid) -> PgResult<()>) -> PgResult<Option<ActiveSeg>> {
    loop {
        let Some(a) = find()? else { return Ok(None) };
        lock(a.relid)?;
        if find()? == Some(a) {
            return Ok(Some(a));
        }
        unlock(a.relid)?;
    }
}

/// Extension is not WAL-logged: a crash leaves zeroed pages the heap inits on use.
fn pre_extend_fsm(seg: &Relation<'_>, pages: u32) -> PgResult<()> {
    // The segment is not live yet, so no one else extends it; one call takes at most 64 buffers.
    let first = bufmgr::RelationGetNumberOfBlocksInFork(seg, ForkNumber::MAIN_FORKNUM)?;
    let mut added = 0;
    while added < pages {
        let (buffer, n) = bufmgr_seams::extend_buffered_rel_by::call(seg, ForkNumber::MAIN_FORKNUM, None, 0, (pages - added).min(64))?;
        bufmgr_seams::release_buffer::call(buffer)?;
        added += n;
    }
    for b in first..first + added {
        freespace_seams::record_page_with_free_space::call(seg, b, BLCKSZ - SizeOfPageHeaderData)?;
    }
    freespace_seams::free_space_map_vacuum_range::call(seg, first, first + added)
}

pub fn with_segments(ext: &[u8], recs: &[SegmentRecord]) -> pgrc_format::FormatResult<Vec<u8>> {
    let mut out = pgrc_format::try_vec(ext.len() + 20 * recs.len(), "pgrc segment ring")?;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_SEGMENTS {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if !recs.is_empty() {
        encode_segments(recs, &mut out);
    }
    Ok(out)
}

pub fn activation_slot(recs: &[SegmentRecord]) -> Option<u16> {
    (0..pgrc_format::rowid::SEG_SLOTS).find(|s| recs.iter().all(|r| r.slot != *s))
}

pub fn activate<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: u16, pre_extend: u32, maint: bool) -> PgResult<()> {
    let relid = match pgrc_front_seams::segment_relid::call(rel.rd_id, slot)? {
        Some(r) => {
            pgrc_front_seams::reset_segment::call(mcx, r)?;
            r
        }
        None => pgrc_front_seams::create_segment::call(mcx, rel.rd_id, slot)?,
    };
    if pre_extend > 0 {
        let seg = table::table_open(mcx, relid, AccessShareLock)?;
        let out = pre_extend_fsm(&seg, pre_extend);
        seg.close(NoLock)?;
        out?;
    }
    let publish = if maint { pgrc_am::ingest::publish_ext_maint } else { pgrc_am::ingest::publish_ext };
    let mut id_err: Option<Box<types_error::PgError>> = None;
    let published = publish(rel, &mut |base, b| {
        let mut recs = base.map(segments).transpose()?.unwrap_or_default();
        recs.retain(|r| r.slot != slot);
        recs.iter_mut().filter(|r| r.state == SegState::Active).for_each(|r| r.state = SegState::Sealing);
        let part_no = pgrc_write::plan::PlanBuilder::reserve(b);
        // before the publish and not transactional: the slot's previous segment's extents go (no reader sees it any
        // more: its slot was reclaimed past the horizon), with any no ring record names (an aborted or retried
        // activation's), and the new segment's first extent is reserved
        let keep: Vec<u32> = recs.iter().map(|r| r.part_no).chain([part_no]).collect();
        if let Err(e) = pgrc_am::rowid_alloc::prune(rel, &|p| keep.contains(&p)).and_then(|_| pgrc_am::rowid_alloc::extend(rel, part_no, 0)) {
            id_err = Some(e);
            return Err(pgrc_write::WriteError::Contract { detail: "segment extent reservation failed (see raised error)" });
        }
        recs.push(SegmentRecord { slot, part_no, relid, state: SegState::Active, fxid: b.fxid() });
        recs.sort_by_key(|r| r.slot);
        b.ext(with_segments(base.map_or(&[][..], |m| &m.ext), &recs)?);
        Ok(())
    });
    published.map_err(|e| id_err.take().unwrap_or(e))?;
    Ok(())
}

/// Block counts come from smgr each time: a cached count can be stale-low.
pub fn front_bearing<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    bearing(mcx, rel, Composed::Ignored)
}

pub fn front_bearing_parts<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    bearing(mcx, rel, Composed::Bears)
}

pub fn front_bearing_served<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<bool> {
    bearing(mcx, rel, Composed::Served)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Composed {
    Ignored,
    Bears,
    Served,
}

fn bearing<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, composed: Composed) -> PgResult<bool> {
    let Some(g) = newest(rel)? else { return Ok(false) };
    let view = statement_view(rel)?;
    if let Some(v) = view.as_ref().filter(|v| composed != Composed::Ignored && v.composed()) {
        if v.stacked() > 0 {
            elog::elog(types_error::DEBUG1, format!("pgrcolumnar: stacked view of \"{}\" gen {}: {} parts hidden or unsettled", rel.name(), v.gen, v.stacked()))?;
        }
        if composed == Composed::Bears {
            return Ok(true);
        }
    }
    for relid in unsealed_relids(records(&g), view.as_deref().map_or(&[][..], records)) {
        let seg = table::table_open(mcx, relid, AccessShareLock)?;
        let n = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
        seg.close(NoLock)?;
        if n? > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unsealed_relids(newest: &[SegmentRecord], view: &[SegmentRecord]) -> Vec<Oid> {
    let mut relids: Vec<Oid> = newest.iter().chain(view).filter(|r| r.state != SegState::Sealed).map(|r| r.relid).collect();
    relids.sort_unstable();
    relids.dedup();
    relids
}

/// The generation the statement's snapshot sees: the active snapshot's, else a REPEATABLE READ or SERIALIZABLE
/// transaction's (Gravity's simple-protocol entry asks before the portal pushes one). `None` when no snapshot is taken
/// yet: the statement's will see the newest generation or a later one.
pub fn statement_view(rel: &Relation<'_>) -> PgResult<Option<Arc<Generation>>> {
    let snap = if snapmgr::ActiveSnapshotSet() {
        snapmgr::GetActiveSnapshot()
    } else if ::xact::IsolationUsesXactSnapshot() && snapmgr::FirstSnapshotSet() {
        snapmgr::GetTransactionSnapshot()?
    } else {
        return Ok(None);
    };
    view_under(rel, &snap)
}

pub(crate) fn view_under(rel: &Relation<'_>, snap: &types_snapshot::SnapshotData<'_>) -> PgResult<Option<Arc<Generation>>> {
    let slot = rel_slot(rel);
    let compute = || -> PgResult<(Option<Arc<Generation>>, Option<u64>)> {
        let check = SnapshotCommitCheck::new(Some(snap));
        let (found, head) = catalog::visible_at_head(slot, &check)?;
        check.take_error()?;
        let view = match found {
            Lookup::Gen(g) => Some(g),
            Lookup::Empty | Lookup::Unknown => None,
        };
        Ok((view, head))
    };
    if snap.snapshot_type != types_snapshot::SnapshotType::SNAPSHOT_MVCC {
        return compute().map(|(v, _)| v);
    }
    let at = crate::pathcache::ViewAt { cid: ::xact::GetCurrentCommandId(false)?, subid: ::xact::GetCurrentSubTransactionId() };
    crate::pathcache::statement_view(rel, at, snap, || catalog::head(slot), compute)
}

pub fn unsealed_blocks<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<u64> {
    let Some(g) = newest(rel)? else { return Ok(0) };
    blocks(mcx, records(&g), &|s| s != SegState::Sealed)
}

pub fn unsealed_shapes<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Vec<(u64, i32, f64)>> {
    let Some(g) = newest(rel)? else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for r in records(&g).iter().filter(|r| r.state != SegState::Sealed) {
        let Some(seg) = table::try_table_open(mcx, r.relid, AccessShareLock)? else { continue };
        let b = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
        let (relpages, reltuples) = (seg.rd_rel.relpages, f64::from(seg.rd_rel.reltuples));
        seg.close(NoLock)?;
        out.push((b? as u64, relpages, reltuples));
    }
    Ok(out)
}

pub fn unsealed_rows<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<u64> {
    let Some(g) = newest(rel)? else { return Ok(0) };
    let snap = snapmgr::GetLatestSnapshot()?;
    let snap = snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered a snapshot");
    let mut n = 0u64;
    let mut out = Ok(());
    for r in records(&g).iter().filter(|r| r.state != SegState::Sealed) {
        let seg = table::table_open(mcx, r.relid, AccessShareLock)?;
        let counted = (|| {
            let flags = tableam_vocab::SO_TYPE_SEQSCAN | tableam_vocab::SO_ALLOW_PAGEMODE;
            let mut scan = heapam::heap_beginscan(mcx, &seg, Some(snap.clone()), 0, mcx::PgVec::new_in(mcx), None, flags)?;
            while heapam::heap_getnext(&mut scan, types_scan::sdir::ScanDirection::ForwardScanDirection)?.is_some() {
                n += 1;
            }
            heapam::heap_endscan(scan)
        })();
        seg.close(NoLock)?;
        if let Err(e) = counted {
            out = Err(e);
            break;
        }
    }
    snapmgr::UnregisterSnapshot(Some(&snap));
    out.map(|()| n)
}

pub fn blocks<'mcx>(mcx: Mcx<'mcx>, recs: &[SegmentRecord], keep: &dyn Fn(SegState) -> bool) -> PgResult<u64> {
    let mut n = 0u64;
    for r in recs.iter().filter(|r| keep(r.state)) {
        let seg = table::table_open(mcx, r.relid, AccessShareLock)?;
        let b = bufmgr::RelationGetNumberOfBlocksInFork(&seg, ForkNumber::MAIN_FORKNUM);
        seg.close(NoLock)?;
        n += b? as u64;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(slot: u16, state: SegState) -> SegmentRecord {
        SegmentRecord { slot, part_no: 10 + slot as u32, relid: 500 + slot as u32, state, fxid: 7 }
    }

    #[test]
    fn activation_takes_the_lowest_slot_no_record_names() {
        assert_eq!(activation_slot(&[]), Some(0));
        assert_eq!(activation_slot(&[seg(0, SegState::Sealed)]), Some(1), "slot 0's sealed segment is kept");
        assert_eq!(activation_slot(&[seg(0, SegState::Sealed), seg(1, SegState::Sealing), seg(3, SegState::Sealed)]), Some(2));
        let full: Vec<SegmentRecord> = (0..pgrc_format::rowid::SEG_SLOTS).map(|s| seg(s, SegState::Sealed)).collect();
        assert_eq!(activation_slot(&full), None);
    }

    #[test]
    fn a_switch_between_lookup_and_lock_retries_on_the_new_segment() {
        let before = [seg(0, SegState::Active)];
        let after = [seg(0, SegState::Sealing), seg(1, SegState::Active)];
        let mut calls = 0;
        let (mut locked, mut unlocked) = (Vec::new(), Vec::new());
        let got = lock_active(
            &mut || {
                calls += 1;
                Ok(active_of(if calls == 1 { &before } else { &after }))
            },
            &mut |r| {
                locked.push(r);
                Ok(())
            },
            &mut |r| {
                unlocked.push(r);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(got.map(|a| a.slot), Some(1));
        assert_eq!((locked, unlocked), (vec![500, 501], vec![500]));
    }

    #[test]
    fn newer_segments_are_the_newest_live_ones_the_view_does_not_list() {
        let view = [seg(0, SegState::Active), seg(2, SegState::Sealed)];
        let mut reused = seg(2, SegState::Active);
        reused.part_no = 40;
        let newest = [seg(0, SegState::Sealing), seg(1, SegState::Active), reused, seg(3, SegState::Sealed)];
        let slots = |v: Vec<SegmentRecord>| v.iter().map(|r| r.slot).collect::<Vec<_>>();
        assert_eq!(slots(unlisted(&view, &newest)), vec![1, 2], "a listed segment is read already; a sealed one is a part; a reused slot is a new segment");
        assert_eq!(slots(unlisted(&[], &newest)), vec![0, 1, 2], "a snapshot that sees no generation");
        assert!(unlisted(&newest, &newest).is_empty());
    }

    #[test]
    fn front_bearing_reads_the_views_unsealed_segments_too() {
        let view = [seg(0, SegState::Active)];
        let newest = [seg(0, SegState::Sealed), seg(1, SegState::Active)];
        assert_eq!(unsealed_relids(&newest, &view), vec![500, 501]);
        assert_eq!(unsealed_relids(&newest, &[]), vec![501], "no snapshot: the newest generation's alone");
        assert_eq!(unsealed_relids(&view, &view), vec![500], "each relid once");
        assert!(unsealed_relids(&[seg(2, SegState::Sealed)], &[seg(2, SegState::Sealed)]).is_empty());
    }

    #[test]
    fn an_empty_ring_has_no_active_segment_and_segments_replace_in_place() {
        assert_eq!(lock_active(&mut || Ok(None), &mut |_| unreachable!(), &mut |_| unreachable!()).unwrap(), None);
        assert_eq!(active_of(&[seg(0, SegState::Sealing)]), None);
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"kept");
        let one = with_segments(&ext, &[seg(0, SegState::Active)]).unwrap();
        let two = with_segments(&one, &[seg(0, SegState::Sealing), seg(1, SegState::Active)]).unwrap();
        let recs = ext_records(&two).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0], (0x7ff0, &b"kept"[..]));
        assert_eq!(with_segments(&two, &[]).unwrap(), ext);
    }
}
