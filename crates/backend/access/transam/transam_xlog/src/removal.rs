use std::cell::Cell;
use std::sync::atomic::Ordering::Relaxed;

use types_core::{TimeLineID, XLogRecPtr, XLogSegNo};
use types_error::PgResult;

use crate::ctl::XLogCtl;
use crate::*;

thread_local! {
    static CHECK_POINT_DISTANCE_ESTIMATE: Cell<f64> = const { Cell::new(0.0) };
    static PREV_CHECK_POINT_DISTANCE: Cell<f64> = const { Cell::new(0.0) };
}

// CheckXLogRemoved (xlog.c): error out if the segment we intend to read has
// already been recycled/removed under us.
pub fn CheckXLogRemoved(segno: XLogSegNo, tli: TimeLineID) -> PgResult<()> {
    let ctl = XLogCtl();
    let last_removed = ctl.info_lck.with(|| ctl.lastRemovedSegNo.load(Relaxed));
    if segno <= last_removed {
        let filename = crate::XLogFileName(tli, segno, wal_segment_size());
        return elog::ereport(types_error::ERROR)
            .errcode_for_file_access()
            .errmsg(format!("requested WAL segment {filename} has already been removed"))
            .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "CheckXLogRemoved"));
    }
    Ok(())
}

// PrevCheckPointDistance / CheckPointDistanceEstimate (xlog.c), read by
// LogCheckpointEnd and XLOGfileslop.
pub(crate) fn prev_check_point_distance() -> f64 {
    PREV_CHECK_POINT_DISTANCE.get()
}
pub(crate) fn check_point_distance_estimate() -> f64 {
    CHECK_POINT_DISTANCE_ESTIMATE.get()
}

pub(crate) fn UpdateCheckPointDistanceEstimate(nbytes: u64) {
    let nbytes = nbytes as f64;
    PREV_CHECK_POINT_DISTANCE.set(nbytes);
    let est = CHECK_POINT_DISTANCE_ESTIMATE.get();
    CHECK_POINT_DISTANCE_ESTIMATE.set(if est < nbytes { nbytes } else { 0.90 * est + 0.10 * nbytes });
}

fn ConvertToXSegs(mb: i32, wal_segsz: i32) -> u64 {
    XLogMBVarToSegs(mb, wal_segsz) as u64
}

pub(crate) fn XLOGfileslop(lastredoptr: XLogRecPtr) -> XLogSegNo {
    let wal_segsz = wal_segment_size();
    let min_seg_no = lastredoptr / wal_segsz as u64
        + ConvertToXSegs(guc_tables::vars::min_wal_size_mb.read(), wal_segsz)
        - 1;
    let max_seg_no = lastredoptr / wal_segsz as u64
        + ConvertToXSegs(guc_tables::vars::max_wal_size_mb.read(), wal_segsz)
        - 1;

    let mut distance = (1.0 + guc_tables::vars::CheckPointCompletionTarget.read())
        * CHECK_POINT_DISTANCE_ESTIMATE.get();
    distance *= 1.10;

    // Sequential bounds: min_wal_size > max_wal_size is legal (max wins).
    let mut recycle_seg_no =
        ((lastredoptr as f64 + distance) / wal_segsz as f64).ceil() as XLogSegNo;
    if recycle_seg_no < min_seg_no {
        recycle_seg_no = min_seg_no;
    }
    if recycle_seg_no > max_seg_no {
        recycle_seg_no = max_seg_no;
    }
    recycle_seg_no
}

// Returns true when the max_slot_wal_keep_size cap pushed the slot horizon
// forward — C must then invalidate the affected slots.
pub(crate) fn KeepLogSeg(recptr: XLogRecPtr, log_seg_no: &mut XLogSegNo) -> PgResult<bool> {
    // WR L4b: the checkpoint's (restartpoint's) redo is the spool's floor; its slot invalidation and
    // RemoveOldXlogFiles, on this thread, read what this pass decided.
    let ctl = XLogCtl();
    let redo = ctl.info_lck.with(|| ctl.RedoRecPtr.load(Relaxed));
    let (slot_horizon_capped, keep) = keep_log_seg_spool(recptr, log_seg_no, redo)?;
    SPOOL_KEEP.set(Some(keep));
    Ok(slot_horizon_capped)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpoolKeep {
    pub(crate) segno: XLogSegNo,
    pub(crate) cut: XLogRecPtr,
    pub(crate) horizon: XLogRecPtr,
}

thread_local! {
    static SPOOL_KEEP: Cell<Option<SpoolKeep>> = const { Cell::new(None) };
}

fn keep_log_seg_spool(recptr: XLogRecPtr, log_seg_no: &mut XLogSegNo, floor: XLogRecPtr) -> PgResult<(bool, SpoolKeep)> {
    let ctl = XLogCtl();
    let keep = ctl.info_lck.with(|| ctl.replicationSlotMinLSN.load(Relaxed));
    let unsummarized = walsummarizer_seams::get_oldest_unsummarized_lsn::call()?;
    Ok(pgrc_spool::with_ledger(|ledger| {
        let (slot_horizon_capped, cut) = keep_log_seg_with(recptr, log_seg_no, keep, ledger);

        // C applies this clamp between the slot cap and wal_keep_size; all three
        // only lower segno, so applying it after keep_log_seg_with is equivalent.
        // The summarizer reads no spool file: the spool cut stays.
        if unsummarized != InvalidXLogRecPtr {
            let seg = XLByteToSeg(unsummarized, wal_segment_size());
            if seg < *log_seg_no {
                *log_seg_no = seg;
            }
        }
        (slot_horizon_capped, spool_keep(*log_seg_no, cut.min(floor), ledger))
    }))
}

/// Under archiving an entry goes with its segment, which the archiver sends first.
pub(crate) fn spool_keep(segno: XLogSegNo, cut: XLogRecPtr, ledger: &pgrc_spool::Ledger) -> SpoolKeep {
    let start = XLogSegNoOffsetToRecPtr(segno, 0, wal_segment_size());
    let cut = if XLogArchivingActive() { cut.min(start) } else { cut };
    let horizon = start.max(ledger.last_below(cut).map_or(0, |e| e + 1));
    SpoolKeep { segno, cut, horizon }
}

/// PG's KeepLogSeg arithmetic with spool bytes counted (WR L4b): a point X retains D(X) = the whole WAL segments from
/// X's to the current one plus the spool entries at or past X. Returns PG's "capped" and the spool cut: the lowest
/// point a slot or wal_keep_size holds. With an empty ledger it is PG's segment arithmetic exactly.
pub(crate) fn keep_log_seg_with(
    recptr: XLogRecPtr,
    log_seg_no: &mut XLogSegNo,
    keep: XLogRecPtr,
    ledger: &pgrc_spool::Ledger,
) -> (bool, XLogRecPtr) {
    let wal_segsz = wal_segment_size();
    let seg = wal_segsz as u64;
    let curr_seg_no = XLByteToSeg(recptr, wal_segsz);
    let retained = |x: XLogRecPtr| (curr_seg_no - x / seg) * seg + ledger.bytes_from(x);
    let mut segno = curr_seg_no;
    let mut cut = recptr;
    let mut slot_horizon_capped = false;

    if keep != InvalidXLogRecPtr && keep < recptr {
        segno = XLByteToSeg(keep, wal_segsz);
        cut = keep;

        let max_slot_keep_mb = guc_tables::vars::max_slot_wal_keep_size_mb.read();
        if max_slot_keep_mb >= 0 && !init_small::globals::IsBinaryUpgrade() {
            let limit = ConvertToXSegs(max_slot_keep_mb, wal_segsz) * seg;
            if retained(keep) > limit {
                let tail = |s: u64| (curr_seg_no - s) * seg + ledger.bytes_from((s + 1) * seg);
                segno = first_seg(segno, curr_seg_no, |s| tail(s) <= limit);
                cut = ledger.cut_to_budget(keep.max(segno * seg), limit - (curr_seg_no - segno) * seg);
                slot_horizon_capped = true;
            }
        }
    }

    let wal_keep_mb = guc_tables::vars::wal_keep_size_mb.read();
    if wal_keep_mb > 0 {
        let need = ConvertToXSegs(wal_keep_mb, wal_segsz) * seg;
        if retained(cut) < need {
            match last_seg(1, curr_seg_no, |s| retained(s * seg) >= need) {
                None => (segno, cut) = (1, 0),
                Some(s) => {
                    let wal = (curr_seg_no - s) * seg;
                    let k = match wal + ledger.bytes_from((s + 1) * seg) >= need {
                        true => (s + 1) * seg,
                        false => ledger.keep_for(s * seg, (s + 1) * seg, need - wal).unwrap_or(s * seg),
                    };
                    (segno, cut) = (segno.min(s), cut.min(k));
                }
            }
        }
    }

    if segno < *log_seg_no {
        *log_seg_no = segno;
    }
    (slot_horizon_capped, cut)
}

/// The lowest `s` in `[lo, hi]` with `ok(s)` (`ok` is false, then true; `ok(hi)` holds).
fn first_seg(mut lo: u64, mut hi: u64, ok: impl Fn(u64) -> bool) -> u64 {
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if ok(mid) { hi = mid } else { lo = mid + 1 }
    }
    lo
}

/// The highest `s` in `[lo, hi]` with `ok(s)` (`ok` is true, then false).
fn last_seg(mut lo: u64, mut hi: u64, ok: impl Fn(u64) -> bool) -> Option<u64> {
    if lo > hi || !ok(lo) {
        return None;
    }
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if ok(mid) { lo = mid } else { hi = mid - 1 }
    }
    Some(lo)
}

pub fn wal_removed_horizon(segno: XLogSegNo) -> XLogRecPtr {
    match SPOOL_KEEP.get() {
        Some(k) if k.segno == segno => k.horizon,
        _ => XLogSegNoOffsetToRecPtr(segno, 0, wal_segment_size()),
    }
}

pub fn wal_keep_horizon(currpos: XLogRecPtr) -> PgResult<XLogRecPtr> {
    let mut segno = XLByteToSeg(currpos, wal_segment_size());
    Ok(keep_log_seg_spool(currpos, &mut segno, currpos)?.1.horizon)
}

pub fn spool_bytes_from(lsn: XLogRecPtr) -> u64 {
    pgrc_spool::with_ledger(|l| l.bytes_from(lsn))
}

/// The newest removed spool entry + 1: a slot that needs a point below it has lost a file.
pub fn spool_removed_upto() -> XLogRecPtr {
    pgrc_spool::with_ledger(|l| l.removed_upto())
}

/// Spool bytes count as WAL written for max_wal_size, so a load of parts by reference requests checkpoints as heap's
/// WAL does. A request can be spent on a checkpoint that CreateCheckPoint skips as idle or on a restartpoint with
/// nothing new to use; the redo then stays, and the next boundary asks again.
pub fn spool_grew(lsn: XLogRecPtr) -> PgResult<()> {
    if !init_small::globals::IsUnderPostmaster() || !checkpointer_seams::request_checkpoint::is_installed() {
        return Ok(());
    }
    let redo = crate::insert::GetRedoRecPtr();
    if lsn <= redo {
        return Ok(());
    }
    let seg = wal_segment_size() as u64;
    let budget = (CheckPointSegments().max(1) as u64 - 1) * seg;
    let (held, own) = pgrc_spool::with_ledger(|l| (l.bytes_from(redo), l.bytes_from(lsn) - l.bytes_from(lsn + 1)));
    if spool_ask(lsn - redo + held, own, budget, seg) {
        checkpointer_seams::request_checkpoint::call(CHECKPOINT_CAUSE_XLOG)?;
    }
    Ok(())
}

/// Whether an entry of `own` bytes that brought the checkpoint distance to `dist` asks for a checkpoint: the distance is
/// at least `budget`, and the entry carried it across a multiple of `seg` (the budget is one).
pub(crate) fn spool_ask(dist: u64, own: u64, budget: u64, seg: u64) -> bool {
    dist >= budget && (dist - own.min(dist)) / seg < dist / seg
}

pub(crate) fn IsXLogFileName(fname: &str) -> bool {
    fname.len() == 24 && fname.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
}

pub(crate) fn IsPartialXLogFileName(fname: &str) -> bool {
    fname.len() == 24 + ".partial".len()
        && fname.ends_with(".partial")
        && IsXLogFileName(&fname[..24])
}

pub(crate) fn XLogFromFileName(fname: &str, wal_segsz: i32) -> (TimeLineID, XLogSegNo) {
    let tli = u32::from_str_radix(&fname[0..8], 16).unwrap_or(0);
    let log = u64::from_str_radix(&fname[8..16], 16).unwrap_or(0);
    let seg = u64::from_str_radix(&fname[16..24], 16).unwrap_or(0);
    (tli, log * XLogSegmentsPerXLogId(wal_segsz) + seg)
}

pub fn XLogGetOldestSegno(tli: TimeLineID) -> PgResult<XLogSegNo> {
    let mut oldest_segno: XLogSegNo = 0;
    let xldir = fd::AllocateDir(XLOGDIR)?;
    while let Some(xlde) = fd::ReadDir(xldir, XLOGDIR)? {
        if !IsXLogFileName(&xlde.d_name) {
            continue;
        }
        let (file_tli, file_segno) = XLogFromFileName(&xlde.d_name, wal_segment_size());
        if tli != file_tli {
            continue;
        }
        if oldest_segno == 0 || file_segno < oldest_segno {
            oldest_segno = file_segno;
        }
    }
    fd::FreeDir(xldir)?;
    Ok(oldest_segno)
}

fn UpdateLastRemovedPtr(fname: &str) {
    let (_, segno) = XLogFromFileName(fname, wal_segment_size());
    let ctl = XLogCtl();
    ctl.info_lck.with(|| {
        if segno > ctl.lastRemovedSegNo.load(Relaxed) {
            ctl.lastRemovedSegNo.store(segno, Relaxed);
        }
    });
}

// archive_mode=off is C's XLogArchiveCheckDone fast path; the fast path also
// keeps seam-less unit tests off the uninstalled-seam panic.
fn xlog_archive_check_done(xlog: &str) -> PgResult<bool> {
    if !XLogArchivingActive() {
        return Ok(true);
    }
    xlogarchive_seams::xlog_archive_check_done::call(xlog)
}

pub(crate) fn xlog_archive_cleanup(xlog: &str) {
    if xlogarchive_seams::xlog_archive_cleanup::is_installed() {
        return xlogarchive_seams::xlog_archive_cleanup::call(xlog);
    }
    let _ = fd::pg_unlink(&format!("{XLOGDIR}/archive_status/{xlog}.done"));
    let _ = fd::pg_unlink(&format!("{XLOGDIR}/archive_status/{xlog}.ready"));
}

// The keep test ignores names' TLI so a parent timeline's segments survive.
pub(crate) fn RemoveOldXlogFiles(
    segno: XLogSegNo,
    lastredoptr: XLogRecPtr,
    endptr: XLogRecPtr,
    insert_tli: TimeLineID,
) -> PgResult<()> {
    let wal_segsz = wal_segment_size();
    let mut endlog_seg_no = XLByteToSeg(endptr, wal_segsz);
    let recycle_seg_no = XLOGfileslop(lastredoptr);

    let lastoff = XLogFileName(0, segno, wal_segsz);
    let _ = elog::elog(
        types_error::DEBUG2,
        format!("attempting to remove WAL segments older than log file {lastoff}"),
    );

    let xldir = fd::AllocateDir(XLOGDIR)?;
    let mut kept_seg_no = segno + 1;
    while let Some(de) = fd::ReadDir(xldir, XLOGDIR)? {
        let fname = de.d_name.as_str();
        if !IsXLogFileName(fname) && !IsPartialXLogFileName(fname) {
            continue;
        }
        // Full-tail compare keeps an equal-segno ".partial" (C strcmp parity).
        if fname[8..] <= lastoff[8..] {
            if xlog_archive_check_done(fname)? {
                UpdateLastRemovedPtr(fname);
                RemoveXlogFile(fname, recycle_seg_no, &mut endlog_seg_no, insert_tli)?;
            } else {
                kept_seg_no = kept_seg_no.min(XLogFromFileName(fname, wal_segsz).1);
            }
        }
    }
    fd::FreeDir(xldir)?;
    // WR L4b: when every segment below the kept one went, the cut this checkpoint's KeepLogSeg made (inside a kept
    // segment when the limits count spool bytes).
    let seg_cut = XLogSegNoOffsetToRecPtr(kept_seg_no, 0, wal_segsz);
    let cutoff = match SPOOL_KEEP.get() {
        Some(k) if kept_seg_no == segno + 1 && k.segno == kept_seg_no => k.cut.min(lastredoptr).max(seg_cut),
        _ => seg_cut,
    };
    injection_point::injection_point("pgrc-before-spool-removal")?;
    remove_spool_below(cutoff);
    Ok(())
}

/// A failure is a WARNING; the next checkpoint retries.
fn remove_spool_below(cutoff: XLogRecPtr) {
    let spool = pgrc_spool::Spool::data();
    match spool.remove_below(cutoff) {
        Ok((0, _)) => {}
        Ok((files, bytes)) => {
            let held = pgrc_spool::with_ledger(|l| l.total());
            let _ = elog::elog(
                types_error::LOG,
                format!("pgrc spool: {files} referenced files removed ({bytes} bytes), {held} bytes still held"),
            );
        }
        Err(e) => {
            let _ = elog::elog(types_error::WARNING, format!("pgrc spool: could not remove old entries: {e}"));
        }
    }
}

// After a timeline switch: drop old-timeline segments past the switch point.
pub fn RemoveNonParentXlogFiles(
    switchpoint: XLogRecPtr,
    new_tli: TimeLineID,
) -> PgResult<()> {
    let wal_segsz = wal_segment_size();
    let switch_seg_no = XLByteToPrevSeg(switchpoint, wal_segsz);
    let mut endlog_seg_no = XLByteToSeg(switchpoint, wal_segsz);
    let recycle_seg_no = endlog_seg_no + 10;

    let switchseg = XLogFileName(new_tli, switch_seg_no, wal_segsz);
    let _ = elog::elog(
        types_error::DEBUG2,
        format!("attempting to remove WAL segments newer than log file {switchseg}"),
    );

    let xldir = fd::AllocateDir(XLOGDIR)?;
    while let Some(de) = fd::ReadDir(xldir, XLOGDIR)? {
        let fname = de.d_name.as_str();
        if !IsXLogFileName(fname) {
            continue;
        }
        if fname[..8] < switchseg[..8] && fname[8..] > switchseg[8..] {
            if !xlogarchive_seams::xlog_archive_is_ready::call(fname) {
                RemoveXlogFile(fname, recycle_seg_no, &mut endlog_seg_no, new_tli)?;
            }
        }
    }
    fd::FreeDir(xldir)?;
    Ok(())
}

// Only regular files recycle — never rename an archive symlink into pg_wal.
fn RemoveXlogFile(
    segname: &str,
    recycle_seg_no: XLogSegNo,
    endlog_seg_no: &mut XLogSegNo,
    insert_tli: TimeLineID,
) -> PgResult<()> {
    let path = format!("{XLOGDIR}/{segname}");

    // lstat: a symlink into pg_wal must never recycle (dirent d_type parity).
    let mut fi = fd::FileInfo::zeroed();
    let is_reg = fd::pg_lstat(&path, &mut fi) == 0 && fi.is_file();
    if guc_tables::vars::wal_recycle.read()
        && *endlog_seg_no <= recycle_seg_no
        && XLogCtl().InstallXLogFileSegmentActive.load(Relaxed)
        && is_reg
        && crate::write::InstallXLogFileSegment(
            endlog_seg_no,
            &path,
            true,
            recycle_seg_no,
            insert_tli,
        )?
    {
        let _ = elog::elog(
            types_error::DEBUG2,
            format!("recycled write-ahead log file \"{segname}\""),
        );
        crate::startup::checkpoint_stats_bump_segs_recycled();
        *endlog_seg_no += 1;
    } else {
        let _ = elog::elog(
            types_error::DEBUG2,
            format!("removing write-ahead log file \"{segname}\""),
        );
        if fd::durable_unlink(&path, types_error::LOG).map(|rc| rc != 0).unwrap_or(true) {
            return Ok(());
        }
        crate::startup::checkpoint_stats_bump_segs_removed();
    }

    xlog_archive_cleanup(segname);
    Ok(())
}
