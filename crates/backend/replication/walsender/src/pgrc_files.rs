// pgrcolumnar parts by reference (WR L3, docs WR.md "L3 — streaming"): a physical walsender sends the file of every
// FILE_REF record ahead of the WAL that holds it, as `'F'` CopyData messages, to a session that opted in with
// `-c pgrust.pgrc_stream_files=on` (OC-3; the pgrust walreceiver always does). Other sessions get WAL only, and the
// first reference past their start is LOGged once (OC-4), except the WAL stream of a base backup (`pg_basebackup -X
// stream`), which is held at the first reference its backup's archive does not carry (WR L4; see
// `pgrc_spool::BackupCover`).
//
//   'F' | ref_lsn u64 BE | file_len u64 BE | offset u64 BE | bytes (<= MAX_SEND_SIZE)
//
// References are found in the spool, not by decoding WAL: after the flush pointer is read, `pgrc_spool::settle`
// waits until every reference below it is named, so a listing up to it is authoritative. Files below the standby's
// reported flush are skipped: a standby flushes WAL past a reference only after its file is durable. Each file is
// followed by the WAL up to the next reference (`file_step`), so a burst of references replays as its files land.
// WAL sent ahead of a reference ends at its page's start when it is the page's first record (`wal_end_before`).

use std::cell::RefCell;
use std::collections::VecDeque;

use elog::ereport;
use pgrc_spool::Spool;
use types_core::{InvalidXLogRecPtr, TimestampTz, XLogRecPtr};
use types_error::{ErrorLocation, PgResult, DEBUG1, ERRCODE_UNDEFINED_FILE, ERROR, LOG};
use types_storage::waiteventset::{WL_SOCKET_READABLE, WL_SOCKET_WRITEABLE};

use crate::streaming::{
    reset_my_latch, WalSndCheckTimeOut, WalSndComputeSleeptime, WalSndKeepalive, WalSndKeepaliveIfNecessary,
    WalSndShutdown, WalSndWait, MAX_SEND_SIZE, WAIT_EVENT_WAL_SENDER_MAIN,
};

/// The session option (OC-3).
pub const STREAM_FILES_GUC: &str = "pgrust.pgrc_stream_files";
/// The `'F'` message type.
pub const MSG_FILE: u8 = b'F';
const HEADER: usize = 1 + 8 + 8 + 8;
// WAIT_EVENT_WAL_SENDER_WRITE_DATA (wait_event_types.h, Client section).
const WAIT_EVENT_WAL_SENDER_WRITE_DATA: u32 = 0x0600_0000 | 8;
/// How long a session waits for the standby's first reply before sending every file from its start point.
const FIRST_REPLY_WAIT_US: TimestampTz = 10_000_000;

fn loc(func: &'static str) -> ErrorLocation {
    ErrorLocation::new("src/backend/replication/pgrc_files.rs", 0, func)
}

fn lsn_fmt(l: XLogRecPtr) -> String {
    format!("{:X}/{:X}", (l >> 32) as u32, l as u32)
}

fn get_ts() -> TimestampTz {
    timestamp_seams::get_current_timestamp::call()
}

#[derive(Default)]
struct FileSender {
    active: bool,
    on: bool,
    listed: bool,
    logged: bool,
    /// Entries (LSN, bytes) in `[sent, cache_to)` not yet sent, sorted.
    cache: VecDeque<(XLogRecPtr, u64)>,
    cache_to: XLogRecPtr,
    /// When the first-reply keepalive was sent (0: not yet).
    asked_at: TimestampTz,
    sent: u64,
    skipped: u64,
    /// The base backup whose WAL this stock stream carries (WR L4).
    backup: Option<std::sync::Arc<pgrc_spool::BackupCover>>,
}

thread_local! {
    static FILES: RefCell<FileSender> = RefCell::new(FileSender::default());
}

fn stream_files_setting() -> bool {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return false;
    }
    let v = guc_seams::get_config_option_missing_ok::call(STREAM_FILES_GUC).ok().flatten().unwrap_or_default();
    matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "yes" | "1")
}

/// A physical START_REPLICATION at `startpoint` begins: read the session's opt-in, forget the previous stream. A
/// stock stream starting where a base backup's stream starts carries that backup's WAL.
pub(crate) fn start(startpoint: XLogRecPtr) {
    let on = stream_files_setting();
    let backup = if on { None } else { pgrc_spool::backup_stream(startpoint) };
    FILES.with(|f| *f.borrow_mut() = FileSender { active: true, on, backup, ..FileSender::default() });
}

/// The end a base backup's stock stream may send to: below the first reference at or past the backup's cover
/// (`limit`), or only below the first reference at all while the backup lists (`None`). `refs` are sorted.
pub(crate) fn backup_hold(refs: impl IntoIterator<Item = XLogRecPtr>, start: XLogRecPtr, end: XLogRecPtr, limit: Option<XLogRecPtr>) -> XLogRecPtr {
    let cover = limit.unwrap_or(0);
    refs.into_iter().find(|&l| l >= start && l < end && l >= cover).unwrap_or(end).max(start)
}

/// One send's files on a streaming session: how many of the sorted pending references `refs` (all at or past the
/// send's start) to take now, and the end to send WAL to. References below the standby's `flush` are skipped (it has
/// their files); the first one at or past it is sent, and the WAL then goes up to the next reference below `end`, so
/// each file is followed by its FILE_REF before the next file goes. Sending every file below `end` first held a
/// parallel COPY's burst of references in one send until the burst's last file had landed.
pub(crate) fn file_step(
    refs: impl IntoIterator<Item = XLogRecPtr>,
    end: XLogRecPtr,
    flush: XLogRecPtr,
) -> (usize, XLogRecPtr) {
    let mut refs = refs.into_iter().take_while(|&l| l < end);
    let mut n = 0;
    for l in refs.by_ref() {
        n += 1;
        if l >= flush {
            return (n, refs.next().map_or(end, wal_end_before));
        }
    }
    (n, end)
}

/// Where WAL sent ahead of the reference at `lsn` ends: at `lsn`, or at its page's start when the reference is the
/// page's first record. No end pointer lies inside a page header (C's XLogBytePosToEndRecPtr): a standby whose flush
/// stops after a header reads that page with only the header, fails it without a message, and restarts its walreceiver.
pub(crate) fn wal_end_before(lsn: XLogRecPtr) -> XLogRecPtr {
    use transam_xlog::{SizeOfXLogLongPHD, SizeOfXLogShortPHD, XLOG_BLCKSZ};
    let first_page = transam_xlog::XLogSegmentOffset(lsn, transam_xlog::wal_segment_size()) < XLOG_BLCKSZ as u32;
    let header = (if first_page { SizeOfXLogLongPHD } else { SizeOfXLogShortPHD }) as u64;
    if lsn % XLOG_BLCKSZ as u64 == header { lsn - header } else { lsn }
}

/// The stream ended: say what was sent (the reconnect leg counts it).
pub(crate) fn stop() {
    let (active, on, sent, skipped) = FILES.with(|f| {
        let mut f = f.borrow_mut();
        let r = (f.active, f.on, f.sent, f.skipped);
        *f = FileSender::default();
        r
    });
    if active && on && (sent > 0 || skipped > 0) {
        let _ = ereport(DEBUG1)
            .errmsg_internal(format!("pgrc walsender: streamed {sent} files, skipped {skipped} below the standby's flush"))
            .finish(loc("pgrc_files_stop"));
    }
}

fn io_error(e: std::io::Error, what: String) -> PgResult<()> {
    ereport(ERROR)
        .with_saved_errno(e.raw_os_error().unwrap_or(0))
        .errcode_for_file_access()
        .errmsg(format!("{what}: %m"))
        .finish(loc("pgrc_before_send"))
}

/// Before sending WAL `[start, end)` (`rqst`: the flush pointer read for this send, `end <= rqst`): send the files
/// of references below `end` that the standby lacks. Returns the end to send to: `end`, or clamped to the first
/// reference while the standby's flush point is not known yet (the first reply is requested).
pub(crate) fn before_send(start: XLogRecPtr, end: XLogRecPtr, rqst: XLogRecPtr) -> PgResult<XLogRecPtr> {
    let spool = Spool::data();
    // Refresh the listing. Every reference below `rqst` began before the flush pointer was read, so after settle it
    // is named; the first listing of a session also sees entries named before this process started.
    let (listed, cache_to) = FILES.with(|f| (f.borrow().listed, f.borrow().cache_to));
    if !listed || rqst > cache_to {
        pgrc_spool::settle();
        let found = if !listed {
            match spool.list(start, u64::MAX) {
                Ok(v) => {
                    if let Some(&(max, _)) = v.last() {
                        pgrc_spool::note_named(max);
                    }
                    v.into_iter().filter(|&(l, _)| l < rqst).collect()
                }
                Err(e) => return io_error(e, "could not list pgrc spool".into()).map(|_| end),
            }
        } else if pgrc_spool::latest_ref_lsn() >= cache_to {
            match spool.list(cache_to, rqst) {
                Ok(v) => v,
                Err(e) => return io_error(e, "could not list pgrc spool".into()).map(|_| end),
            }
        } else {
            Vec::new()
        };
        FILES.with(|f| {
            let mut f = f.borrow_mut();
            f.listed = true;
            f.cache.extend(found);
            f.cache_to = f.cache_to.max(rqst);
        });
    }

    let (on, first) = FILES.with(|f| {
        let mut f = f.borrow_mut();
        while f.cache.front().is_some_and(|&(l, _)| l < start) {
            f.cache.pop_front();
        }
        (f.on, f.cache.front().copied().filter(|&(l, _)| l < end))
    });
    let Some((first_lsn, _)) = first else { return Ok(end) };

    if !on {
        let backup = FILES.with(|f| f.borrow().backup.clone());
        if let Some(b) = backup {
            // A base backup's stream: WAL only up to the first reference its archive lacks (WR L4).
            let limit = b.limit();
            let (hold, first_hold) = FILES.with(|f| {
                let mut f = f.borrow_mut();
                let hold = backup_hold(f.cache.iter().map(|&(l, _)| l), start, end, limit);
                while f.cache.front().is_some_and(|&(l, _)| l < hold) {
                    f.cache.pop_front();
                }
                (hold, hold < end && limit.is_some() && !std::mem::replace(&mut f.logged, true))
            });
            if first_hold {
                let _ = ereport(DEBUG1)
                    .errmsg_internal(format!("pgrc walsender: base backup stream held at {} (the backup's files end there)", lsn_fmt(hold)))
                    .finish(loc("pgrc_before_send"));
            }
            return Ok(hold);
        }
        // OC-4: WAL only; say once where this client's WAL starts naming files it will not get.
        let log = FILES.with(|f| {
            let mut f = f.borrow_mut();
            while f.cache.front().is_some_and(|&(l, _)| l < end) {
                f.cache.pop_front();
            }
            !std::mem::replace(&mut f.logged, true)
        });
        if log {
            let _ = ereport(LOG)
                .errmsg(format!(
                    "WAL past {} references pgrcolumnar files this replication client will not receive",
                    lsn_fmt(first_lsn)
                ))
                .errhint(format!(
                    "Recovery from this client's WAL needs each file from another source. Connect with \"options=-c {STREAM_FILES_GUC}=on\" to receive them, or set pgrust.pgrc_parts_in_wal = bytes."
                ))
                .finish(loc("pgrc_before_send"));
        }
        return Ok(end);
    }

    // The standby's flush point: its first reply (sent as soon as streaming starts) carries it.
    let mut flush = crate::my_flush();
    if flush == InvalidXLogRecPtr {
        let asked_at = FILES.with(|f| f.borrow().asked_at);
        let now = get_ts();
        if asked_at == 0 {
            WalSndKeepalive(true, InvalidXLogRecPtr)?;
            FILES.with(|f| f.borrow_mut().asked_at = now);
            return Ok(wal_end_before(first_lsn).max(start));
        }
        if now - asked_at < FIRST_REPLY_WAIT_US {
            return Ok(wal_end_before(first_lsn).max(start));
        }
        flush = start;
    }

    let (n, to) = FILES.with(|f| file_step(f.borrow().cache.iter().map(|&(l, _)| l), end, flush));
    for _ in 0..n {
        let Some((lsn, len)) = FILES.with(|f| f.borrow().cache.front().copied()) else { break };
        if lsn >= flush {
            send_file(&spool, lsn, len)?;
        }
        FILES.with(|f| {
            let mut f = f.borrow_mut();
            f.cache.pop_front();
            if lsn >= flush {
                f.sent += 1;
            } else {
                f.skipped += 1;
            }
        });
    }
    Ok(to)
}

/// Nothing could be sent (clamped at the start point): wait for the standby's reply or new WAL.
pub(crate) fn wait_for_reply() -> PgResult<()> {
    let sleep = WalSndComputeSleeptime(get_ts()).min(100);
    WalSndWait(WL_SOCKET_READABLE, sleep, WAIT_EVENT_WAL_SENDER_MAIN)
}

fn send_file(spool: &Spool, lsn: XLogRecPtr, len: u64) -> PgResult<()> {
    let name = pgrc_format::spool::spool_name(lsn);
    let mut r = match spool.open_entry(lsn) {
        Ok(r) => r,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            return ereport(ERROR)
                .errcode(ERRCODE_UNDEFINED_FILE)
                .errmsg(format!("requested pgrc file {name} has already been removed"))
                .finish(loc("pgrc_send_file"));
        }
        Err(e) => return io_error(e, format!("could not open pgrc spool file \"{name}\"")),
    };
    let mut msg = vec![0u8; HEADER + MAX_SEND_SIZE];
    let mut off = 0u64;
    loop {
        let want = (len - off).min(MAX_SEND_SIZE as u64) as usize;
        let got = match r.read_at(off, &mut msg[HEADER..HEADER + want]) {
            Ok(n) => n,
            Err(e) => return io_error(e, format!("could not read pgrc spool file \"{name}\"")),
        };
        if got < want {
            return ereport(ERROR)
                .errcode(ERRCODE_UNDEFINED_FILE)
                .errmsg(format!("pgrc spool file \"{name}\" is shorter than its {len} bytes"))
                .finish(loc("pgrc_send_file"));
        }
        msg[0] = MSG_FILE;
        msg[1..9].copy_from_slice(&lsn.to_be_bytes());
        msg[9..17].copy_from_slice(&len.to_be_bytes());
        msg[17..25].copy_from_slice(&off.to_be_bytes());
        pqcomm::pq_putmessage_noblock(b'd', &msg[..HEADER + want])?;
        drain()?;
        off += want as u64;
        if off >= len {
            break;
        }
    }
    let _ = ereport(DEBUG1)
        .errmsg_internal(format!("pgrc walsender: sent file {name} ({len} bytes)"))
        .finish(loc("pgrc_send_file"));
    Ok(())
}

/// ProcessPendingWrites' loop: a file is many messages, so the output buffer is drained between them.
fn drain() -> PgResult<()> {
    loop {
        if pqcomm::pq_flush_if_writable()? != 0 {
            WalSndShutdown();
        }
        if !pqcomm::pq_is_send_pending() {
            return Ok(());
        }
        crate::replies::ProcessRepliesIfAny()?;
        WalSndCheckTimeOut();
        WalSndKeepaliveIfNecessary()?;
        let sleep = WalSndComputeSleeptime(get_ts());
        WalSndWait(WL_SOCKET_WRITEABLE | WL_SOCKET_READABLE, sleep, WAIT_EVENT_WAL_SENDER_WRITE_DATA)?;
        reset_my_latch();
        postgres_seams::check_for_interrupts::call()?;
    }
}

#[cfg(test)]
mod tests {
    use super::{backup_hold, file_step, wal_end_before};

    /// What a streaming session puts on the wire for pending references `refs` while it sends WAL `[start, end)` in
    /// sends of at most `max` bytes (XLogSendPhysical's loop around before_send): `F<lsn>` for a file, `W<to>` for
    /// the WAL sent up to `to`.
    fn wire(refs: &[u64], mut start: u64, end: u64, max: u64, flush: u64) -> Vec<String> {
        let (mut pending, mut out) = (refs.to_vec(), Vec::new());
        while start < end {
            let chunk = (start + max).min(end);
            let (n, to) = file_step(pending.iter().copied(), chunk, flush);
            out.extend(pending.drain(..n).filter(|&l| l >= flush).map(|l| format!("F{l}")));
            assert!(to > start && to <= chunk, "no progress: {start} -> {to}");
            out.push(format!("W{to}"));
            start = to;
        }
        out
    }

    #[test]
    fn a_burst_of_references_in_one_send_goes_file_then_its_wal() {
        // A parallel COPY's burst: five references inside one send. Each file goes, then the WAL up to the next
        // reference, so the standby replays each FILE_REF as its file lands (not all five files ahead of all WAL).
        let burst = [1000, 1100, 1200, 1300, 1400];
        let each = ["F1000", "W1100", "F1100", "W1200", "F1200", "W1300", "F1300", "W1400", "F1400", "W2000"];
        assert_eq!(wire(&burst, 900, 2000, 131_072, 0), each);
        // Files the standby has (below its flush) are skipped without a WAL stop of their own.
        assert_eq!(wire(&burst, 900, 2000, 131_072, 1250), ["F1300", "W1400", "F1400", "W2000"]);
        // Sends of at most 500 bytes: the chunk end still bounds the WAL, and a reference past it waits its send.
        assert_eq!(wire(&[1000, 1600], 900, 2000, 500, 0), ["F1000", "W1400", "F1600", "W1900", "W2000"]);
        assert_eq!(file_step([], 2000, 0), (0, 2000));
        assert_eq!(file_step([2000, 2100], 2000, 0), (0, 2000), "references at or past the end wait");
    }

    #[test]
    fn a_reference_first_on_its_page_stops_the_wal_at_the_page_start() {
        // WALCUT (RTKEEPS, q047g / q047fl): the FILE_REF at 0/300C018 was the first record of page 0/300C000, so the
        // WAL sent ahead of its file ended right after the page header. The standby's startup read that page with 24
        // bytes, failed without a message and restarted its walreceiver. No end pointer lies inside a page header (C:
        // XLogBytePosToEndRecPtr): the WAL stops at the page start.
        const SEG: u64 = 16 << 20;
        assert_eq!(file_step([0x300_A0B8, 0x300_C018], 0x301_0000, 0), (1, 0x300_C000), "short page header");
        // A segment's first page has the long header.
        assert_eq!(file_step([4 * SEG - 0x2000, 4 * SEG + 40], 4 * SEG + 0x1_0000, 0), (1, 4 * SEG), "long header");
        // A reference after the page's first record: the WAL goes up to it.
        assert_eq!(file_step([0x300_A0B8, 0x300_C060], 0x301_0000, 0), (1, 0x300_C060));
        // On the wire: the WAL to the page start, the file, then the page.
        let each = ["F50372792", "W50380800", "F50380824", "W50397184"];
        assert_eq!(wire(&[0x300_A0B8, 0x300_C018], 0x300_8000, 0x301_0000, 131_072, 0), each);
        // The first-reply clamp: the first reference, or its page's start.
        assert_eq!(wal_end_before(0x300_C018), 0x300_C000);
        assert_eq!(wal_end_before(4 * SEG + 40), 4 * SEG);
        assert_eq!(wal_end_before(4 * SEG + 24), 4 * SEG + 24, "inside a long header is no record start");
        assert_eq!(wal_end_before(0x300_C060), 0x300_C060);
    }

    #[test]
    fn a_backup_stream_is_held_at_the_first_reference_its_archive_lacks() {
        let refs = [100, 200, 300];
        // Copying: nothing held.
        assert_eq!(backup_hold(refs, 50, 400, Some(u64::MAX)), 400);
        // Listing: WAL up to the first reference only.
        assert_eq!(backup_hold(refs, 50, 400, None), 100);
        assert_eq!(backup_hold(refs, 100, 400, None), 100, "held at the start: nothing to send");
        // Covered below 250: references 100 and 200 are in the archive, 300 is not.
        assert_eq!(backup_hold(refs, 50, 400, Some(250)), 300);
        assert_eq!(backup_hold(refs, 50, 280, Some(250)), 280, "the hold is past this send");
        assert_eq!(backup_hold(refs, 300, 400, Some(250)), 300);
        assert_eq!(backup_hold(refs, 350, 400, Some(250)), 400, "references before the start are sent");
        assert_eq!(backup_hold(std::iter::empty(), 50, 400, None), 400);
    }
}
