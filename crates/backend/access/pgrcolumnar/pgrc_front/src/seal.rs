//! The drain and the seal: committed rows in TID order become the segment's part.

use std::sync::Arc;

use mcx::{Mcx, PgVec};
use pgrc_am::ingest::SealWriter;
use pgrc_format::manifest::{segments, SegState, SegmentRecord};
use pgrc_format::rowid::{seg_id, seg_local, SEG_EXTENT_IDS};
use pgrc_write::seal::ColumnPool;
use tableam_vocab::{SO_ALLOW_PAGEMODE, SO_TYPE_SEQSCAN};
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_LOCK_NOT_AVAILABLE, ERROR};
use types_rel::{NoLock, Relation, ShareLock};
use types_storage::lock::LOCKTAG;
use types_scan::sdir::ScanDirection;
use types_tuple::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumberNoCheck};
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use crate::ring;

const PAUSE_GUC: &str = "pgrust.pgrc_seal_pause_ms";

pub fn seal_all<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<(u32, u32)> {
    seal_some(mcx, rel, u32::MAX)
}

pub fn seal_one_waiting<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, targets: &[(Oid, u32)]) -> PgResult<bool> {
    let Some(g) = ring::newest(rel)? else { return Ok(false) };
    let recs = ring::records(&g);
    let Some(rec) = recs.iter().find(|r| r.state == SegState::Sealing && targets.contains(&(r.relid, r.part_no))).copied() else { return Ok(false) };
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    if seal_as(mcx, rel, rec, pool, true, true)? != Sealed::Yes {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: VACUUM could not drain segment slot {} of \"{}\"", rec.slot, rel.name()))));
    }
    let left: Vec<SegmentRecord> = recs.iter().filter(|r| r.state == SegState::Sealing && (r.relid, r.part_no) != (rec.relid, rec.part_no)).copied().collect();
    crate::maint::set_sealing(rel.rd_id, ring::blocks(mcx, &left, &|_| true)?);
    Ok(true)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Seals {
    pub sealed: u32,
    pub skipped: u32,
    pub over_budget: u32,
}

/// A seal neither waits nor defers for another transaction's publish in progress: it builds its part, then chains on
/// that generation (its parts tagged with their creator).
pub fn seal_some<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, limit: u32) -> PgResult<(u32, u32)> {
    seals(mcx, rel, limit, true).map(|s| (s.sealed, s.skipped))
}

pub fn seal_some_nowait<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, limit: u32) -> PgResult<Seals> {
    seals(mcx, rel, limit, false)
}

fn seals<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, limit: u32, wait: bool) -> PgResult<Seals> {
    let Some(g) = ring::newest(rel)? else { return Ok(Seals::default()) };
    let pool = pgrc_front_seams::seal_pool::is_installed().then(pgrc_front_seams::seal_pool::call).flatten();
    let mut out = Seals::default();
    let mut left = Vec::new();
    for rec in ring::records(&g).iter().filter(|r| r.state == SegState::Sealing) {
        if out.sealed == limit || out.over_budget > 0 {
            left.push(*rec);
            continue;
        }
        match seal(mcx, rel, *rec, pool.clone(), wait)? {
            Sealed::Yes => out.sealed += 1,
            Sealed::DrainHeld => {
                out.skipped += 1;
                left.push(*rec);
            }
            Sealed::OverBudget => {
                out.over_budget += 1;
                left.push(*rec);
            }
        }
    }
    crate::maint::set_sealing(rel.rd_id, ring::blocks(mcx, &left, &|_| true)?);
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sealed {
    Yes,
    DrainHeld,
    OverBudget,
}

/// A reservation is all or nothing and a holder never waits for one, so a wait only outlasts that bound when seals hold
/// the budget that long.
fn seal_mem(rel: &Relation<'_>, blocks: u64, wait: bool) -> PgResult<Option<crate::maint::SealMem>> {
    let est = crate::maint::seal_mem_estimate(rel.rd_id, blocks);
    let t0 = pg_clock::MonoStamp::now();
    loop {
        if let Some(m) = crate::maint::try_seal_mem(est) {
            return Ok(Some(m));
        }
        if !wait {
            return Ok(None);
        }
        if t0.elapsed_ns() >= SEAL_MEM_WAIT_S * 1_000_000_000 {
            let msg = format!("pgrcolumnar: seal of \"{}\" ({est} bytes) waited {SEAL_MEM_WAIT_S} s for the seal memory budget ({} of {} in use); sealing over it", rel.name(), crate::maint::seal_mem_in_use(), crate::maint::seal_mem_budget());
            elog::elog(types_error::LOG, msg)?;
            return Ok(Some(crate::maint::force_seal_mem(est)));
        }
        if latch_seams::wait_latch_my_latch::call(WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, 10, PG_WAIT_EXTENSION) & WL_LATCH_SET != 0 {
            latch_seams::reset_latch_my_latch::call();
        }
        postgres_seams::check_for_interrupts::call()?;
    }
}

const PACE_BURST: u64 = 1 << 20;
const PACE_SLICE_NS: u64 = 50_000_000;

/// The rest of the seal goes unpaced as soon as pacing could hold anyone: a full segment waits behind it (the active
/// segment reached the rotation size: the next one would back inserters up), someone waits for a lock the worker holds
/// (a `pgrc_flush`, VACUUM), or an interrupt is pending (it is served at the seal's next check).
fn pace_scope(parent: types_core::Oid, rate: u64, paced: std::rc::Rc<std::cell::Cell<u64>>) -> pgrc_write::walsink::pace::Scope {
    use pgrc_write::walsink::pace;
    let start = pg_clock::MonoStamp::now();
    let mut bucket = pace::Bucket::new(rate, PACE_BURST, 0);
    let mut off = false;
    pace::scope(Box::new(move |bytes| {
        if off {
            return;
        }
        let mut wait = bucket.after(bytes, start.elapsed_ns());
        while wait > 0 {
            if init_small::globals::InterruptPending()
                || crate::maint::rotation_due(parent)
                || crate::maint::lock_waiting(parent)
            {
                off = true;
                return;
            }
            let t = pg_clock::MonoStamp::now();
            let ms = wait.min(PACE_SLICE_NS).div_ceil(1_000_000) as i64;
            if latch_seams::wait_latch_my_latch::call(WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, ms, PG_WAIT_EXTENSION) & WL_LATCH_SET != 0 {
                latch_seams::reset_latch_my_latch::call();
            }
            let spent = t.elapsed_ns();
            paced.set(paced.get() + spent);
            wait = wait.saturating_sub(spent.max(1));
        }
    }))
}

const SEAL_MEM_WAIT_S: u64 = 120;
const PG_WAIT_EXTENSION: u32 = 0x0700_0000;

/// The drain is a conditional ShareLock: it waits out no one and is skipped while a pre-switch inserter is open.
pub fn seal<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, rec: SegmentRecord, pool: Option<Arc<dyn ColumnPool>>, wait: bool) -> PgResult<Sealed> {
    seal_as(mcx, rel, rec, pool, wait, false)
}

fn seal_as<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, rec: SegmentRecord, pool: Option<Arc<dyn ColumnPool>>, wait: bool, drain_wait: bool) -> PgResult<Sealed> {
    let drained = match drain_wait {
        false => lmgr::ConditionalLockRelationOid(rec.relid, ShareLock)?,
        true => drain_waiting(mcx, rec.relid)?,
    };
    if !drained {
        return Ok(Sealed::DrainHeld);
    }
    let seg = table::table_open(mcx, rec.relid, NoLock)?;
    let (t0, blocks) = (std::time::Instant::now(), bufmgr::RelationGetNumberOfBlocksInFork(&seg, types_core::ForkNumber::MAIN_FORKNUM)?);
    let Some(_mem) = seal_mem(rel, u64::from(blocks), wait)? else {
        seg.close(NoLock)?;
        return Ok(Sealed::OverBudget);
    };
    if !wait {
        injection_point::injection_point("pgrc-front-seal-start")?;
    }
    let paced = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let rate = if wait { 0 } else { crate::maint::seal_wal_rate() };
    let _pace = (rate > 0).then(|| pace_scope(rel.rd_id, rate, paced.clone()));
    let mut w = SealWriter::open(rel, pool)?;
    let fed = feed(mcx, rel, &seg, &mut w);
    seg.close(NoLock)?;
    let fed_ms = t0.elapsed().as_millis();
    let positions = match fed {
        Ok(p) => p,
        Err(e) => {
            w.abort()?;
            return Err(e);
        }
    };
    let nrows = positions.rows;
    let bytes = match w.buffered_bytes() {
        Ok(b) => b,
        Err(e) => {
            w.abort()?;
            return Err(e);
        }
    };
    // the part homes the segment's extents (every row's ID is in one: the inserter reserved it first)
    let mut bases = pgrc_am::rowid_alloc::extents(&pgrc_am::rowid_alloc::dir_of(rel))?.bases(rec.part_no);
    // a segment written before row IDs (no extents at all) gets them now, as its inserters would have: its rows read
    // with an invalid TID until this seal, so nothing has seen another ID
    if bases.is_empty() {
        if let Some(last) = positions.last() {
            if let Err(e) = pgrc_am::rowid_alloc::extend(rel, rec.part_no, (u64::from(last) / pgrc_format::rowid::SEG_EXTENT_IDS) as u32) {
                w.abort()?;
                return Err(e);
            }
            bases = pgrc_am::rowid_alloc::extents(&pgrc_am::rowid_alloc::dir_of(rel))?.bases(rec.part_no);
        }
    }
    let ids = match pgrc_am::rowid_alloc::extent_ranges(&bases) {
        Some(ids) if positions.last().is_none_or(|l| seg_id(&bases, l).is_some()) => ids,
        _ => {
            w.abort()?;
            return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} of \"{}\" has rows past its extents", rec.part_no, rel.name())).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)));
        }
    };
    let runs = match positions.id_runs(&bases) {
        Ok(r) => r,
        Err(e) => {
            w.abort()?;
            return Err(e);
        }
    };
    let map = match runs.map(|r| pgrc_format::rowmap::encode_runs(&r, pgrc_format::rowmap::Opts::default())) {
        Some(Ok(m)) => m,
        _ => {
            w.abort()?;
            return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} of \"{}\": its row map does not encode", rec.part_no, rel.name())).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)));
        }
    };
    w.publish(rel, rec.part_no, &map, &ids, &mut |base, b| {
        let mut recs = base.map(segments).transpose()?.unwrap_or_default();
        // re-checked on every try (a publish that waited out another transaction's redoes this edit on the new base)
        let mut flipped = 0;
        for r in recs.iter_mut().filter(|r| r.slot == rec.slot && r.part_no == rec.part_no && r.state == SegState::Sealing) {
            r.state = SegState::Sealed;
            flipped += 1;
        }
        if flipped == 0 {
            return Err(pgrc_write::WriteError::Contract { detail: "a seal's segment is no longer Sealing in the base generation" });
        }
        b.ext(ring::with_segments(base.map_or(&[][..], |m| &m.ext), &recs)?);
        Ok(())
    })?;
    for _ in 0..crate::maint::setting(PAUSE_GUC).unwrap_or(0).div_ceil(10) {
        postgres_seams::check_for_interrupts::call()?;
        waiter::sleep(std::time::Duration::from_millis(10));
    }
    crate::maint::sealed(init_small::globals::MyDatabaseId(), rel.rd_id);
    crate::maint::sealed_shape(rel.rd_id, nrows, u64::from(blocks), bytes);
    crate::maint::sealed_rows(rel.rd_id, nrows);
    let paced_ms = paced.get() / 1_000_000;
    crate::maint::note_paced(rel.rd_id, paced_ms);
    let msg = format!("pgrcolumnar: sealed slot {} of \"{}\": {blocks} blocks, {} rows, feed {fed_ms} ms, total {} ms; {bytes} buffer bytes, rotate at {} blocks, seal memory {} in use, paced {paced_ms} ms", rec.slot, rel.name(), nrows, t0.elapsed().as_millis(), crate::maint::rotate_blocks(rel.rd_id), crate::maint::seal_mem_in_use());
    elog::elog(types_error::DEBUG1, msg)?;
    Ok(Sealed::Yes)
}

const DRAIN_TRIES: u32 = 50;
const DRAIN_QUEUED_MS: i32 = 200;

/// A manual VACUUM's drain, which no INSERT queues behind for long. The VACUUM's rotation committed first, so new
/// inserts go to the new active segment; the holders left are inserters that began before it.
/// A busy trickle can always hold the lock for a millisecond, so then a queued attempt bounded by [`DRAIN_QUEUED_MS`]
/// (the inserters queued behind it wait for millisecond holders only: the first wait outlasted the long ones), and
/// again until it is granted or the user's lock_timeout is spent.
fn drain_waiting(mcx: Mcx<'_>, relid: Oid) -> PgResult<bool> {
    if lmgr::ConditionalLockRelationOid(relid, ShareLock)? {
        return Ok(true);
    }
    let tag = LOCKTAG::relation(init_small::globals::MyDatabaseId(), relid);
    lmgr::WaitForLockersMultiple(mcx, &[tag], ShareLock, false)?;
    let user = lmgr_proc::globals::LockTimeout();
    let t0 = pg_clock::MonoStamp::now();
    loop {
        for i in 0..DRAIN_TRIES {
            if lmgr::ConditionalLockRelationOid(relid, ShareLock)? {
                return Ok(true);
            }
            postgres_seams::check_for_interrupts::call()?;
            waiter::sleep(std::time::Duration::from_millis(1 + u64::from(i % 5)));
        }
        if user > 0 {
            let spent = i32::try_from(t0.elapsed_ms()).unwrap_or(i32::MAX);
            let left = user.saturating_sub(spent);
            if left <= DRAIN_QUEUED_MS {
                // the user's lock_timeout ends within this attempt: its error is the user's
                with_lock_timeout(left.max(1), &mut || lmgr::LockRelationOid(relid, ShareLock))?;
                return Ok(true);
            }
        }
        if queued_attempt(relid, DRAIN_QUEUED_MS)? {
            return Ok(true);
        }
    }
}

fn queued_attempt(relid: Oid, ms: i32) -> PgResult<bool> {
    let owner = resowner::CurrentResourceOwner();
    let held = (init_small::globals::InterruptHoldoffCount(), init_small::globals::QueryCancelHoldoffCount(), init_small::globals::CritSectionCount());
    xact::BeginInternalSubTransaction(Some("pgrc drain"))?;
    let got = with_lock_timeout(ms, &mut || lmgr::LockRelationOid(relid, ShareLock));
    let out = match got {
        Ok(()) => xact::ReleaseCurrentSubTransaction().map(|()| true),
        Err(e) => {
            let timed_out = e.level == ERROR && e.sqlstate == ERRCODE_LOCK_NOT_AVAILABLE;
            if e.level <= ERROR {
                // as a PG_CATCH after errfinish: the counters as they were at the subtransaction's start
                elog::panic_on_crit_section_escape(&e);
                init_small::globals::SetInterruptHoldoffCount(held.0);
                init_small::globals::SetQueryCancelHoldoffCount(held.1);
                init_small::globals::SetCritSectionCount(held.2);
            }
            let rolled = xact::RollbackAndReleaseCurrentSubTransaction();
            resowner::SetCurrentResourceOwner(owner);
            rolled?;
            return if timed_out { Ok(false) } else { Err(e) };
        }
    };
    resowner::SetCurrentResourceOwner(owner);
    out
}

fn with_lock_timeout<T>(ms: i32, f: &mut dyn FnMut() -> PgResult<T>) -> PgResult<T> {
    struct Restore(i32);
    impl Drop for Restore {
        fn drop(&mut self) {
            lmgr_proc::globals::set_LockTimeout(self.0);
        }
    }
    let _restore = Restore(lmgr_proc::globals::LockTimeout());
    lmgr_proc::globals::set_LockTimeout(ms);
    f()
}

#[derive(Default)]
struct LocalRuns {
    runs: Vec<(u32, u32)>,
    rows: u64,
}

impl LocalRuns {
    fn push(&mut self, p: u32) {
        match self.runs.last_mut() {
            Some((s, l)) if u64::from(*s) + u64::from(*l) == u64::from(p) => *l += 1,
            _ => self.runs.push((p, 1)), // alloc: Cold (one per run of rows)
        }
        self.rows += 1;
    }

    fn last(&self) -> Option<u32> {
        self.runs.last().map(|&(s, l)| s + (l - 1))
    }

    fn id_runs(&self, bases: &[u64]) -> PgResult<Option<Vec<(u64, u32)>>> {
        let mut out = ::mcx::stdvec::try_vec(self.runs.len(), "pgrc seal row map")?;
        for &(s, l) in &self.runs {
            let (mut at, end) = (u64::from(s), u64::from(s) + u64::from(l));
            while at < end {
                let room = end.min((at / SEG_EXTENT_IDS + 1) * SEG_EXTENT_IDS) - at;
                let Some(id) = u32::try_from(at).ok().and_then(|a| seg_id(bases, a)) else { return Ok(None) };
                out.push((id, room as u32));
                at += room;
            }
        }
        Ok(Some(out))
    }
}

/// A snapshot after the drain sees every row that will ever commit; no sync scan, so rows come in TID order.
fn feed<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, seg: &Relation<'mcx>, w: &mut SealWriter) -> PgResult<LocalRuns> {
    let snap = snapmgr::GetLatestSnapshot()?;
    let snap = snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered a snapshot");
    let mut scan = heapam::heap_beginscan(mcx, seg, Some(snap.clone()), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE)?;
    let natts = rel.rd_att.natts as usize;
    let (mut values, mut isnull) = (::mcx::stdvec::try_filled(natts, datum::Datum::null(), "pgrc seal feed")?, ::mcx::stdvec::try_filled(natts, true, "pgrc seal feed")?);
    let mut positions = LocalRuns::default();
    let mut out = Ok(());
    while let Some(tup) = heapam::heap_getnext(&mut scan, ScanDirection::ForwardScanDirection)? {
        types_tuple::heap_deform_tuple(tup, &seg.rd_att, &mut values, &mut isnull);
        let (block, off) = (ItemPointerGetBlockNumber(&tup.t_self), ItemPointerGetOffsetNumberNoCheck(&tup.t_self));
        let Some(pos) = seg_local(block, off) else {
            out = Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} row ({block},{off}) past the position range", seg.rd_id))));
            break;
        };
        positions.push(pos);
        let mut held: Vec<PgVec<'mcx, u8>> = Vec::new();
        for c in 0..natts {
            if isnull[c] || rel.rd_att.attr(c).attlen != -1 {
                continue;
            }
            let p = values[c].as_u64() as *const u8;
            // SAFETY: a deformed varlena datum points at a live image in the pinned page.
            if unsafe { types_tuple::varatt::varatt_is_1b_e(p) } {
                let img = unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) };
                held.push(detoast::detoast_attr(mcx, img)?);
                values[c] = datum::Datum::from_u64(held.last().expect("pushed").as_ptr() as u64);
            }
        }
        if let Err(e) = w.append(rel, &values, &isnull) {
            out = Err(e);
            break;
        }
    }
    heapam::heap_endscan(scan)?;
    snapmgr::UnregisterSnapshot(Some(&snap));
    out.map(|()| positions)
}
