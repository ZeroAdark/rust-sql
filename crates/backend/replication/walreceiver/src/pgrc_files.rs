// pgrcolumnar parts by reference (WR L3, docs WR.md "L3 — streaming"): the walsender's `'F'` messages carry the
// file of a FILE_REF record ahead of the WAL that holds it.
//
//   'F' | ref_lsn u64 BE | file_len u64 BE | offset u64 BE | bytes
//
// A file is written to `pg_pgrc_spool/<lsn>.recv` in order from offset 0 and, once complete, fdatasynced, renamed
// to `<lsn>` and the spool fsynced. Messages are processed in order, so the file is durable before any later WAL is
// written, and so before this standby can report a flush past its reference ("lands before it acks").

use std::cell::RefCell;

use elog::ereport;
use pgrc_spool::{Landing, Spool};
use types_error::{ErrorLocation, PgResult, DEBUG1, ERRCODE_PROTOCOL_VIOLATION, ERROR};

pub(crate) const STREAM_FILES_OPTION: &str = "-c pgrust.pgrc_stream_files=on";
const HEADER: usize = 8 + 8 + 8;

fn loc(func: &'static str) -> ErrorLocation {
    ErrorLocation::new("src/backend/replication/walreceiver_pgrc_files.rs", 0, func)
}

struct Receiving {
    landing: Landing,
    len: u64,
    next: u64,
}

thread_local! {
    static RECV: RefCell<Option<Receiving>> = const { RefCell::new(None) };
}

fn violation(what: String) -> PgResult<()> {
    ereport(ERROR)
        .errcode(ERRCODE_PROTOCOL_VIOLATION)
        .errmsg_internal(format!("invalid pgrc file message received from primary: {what}"))
        .finish(loc("pgrc_on_file_chunk"))
}

fn io_error(e: std::io::Error, lsn: u64) -> PgResult<()> {
    ereport(ERROR)
        .with_saved_errno(e.raw_os_error().unwrap_or(0))
        .errcode_for_file_access()
        .errmsg(format!("could not receive pgrc file {}: %m", pgrc_format_name(lsn)))
        .finish(loc("pgrc_on_file_chunk"))
}

fn pgrc_format_name(lsn: u64) -> String {
    format!("{lsn:016X}")
}

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().expect("8 bytes"))
}

pub(crate) fn starts_file(buf: &[u8]) -> bool {
    buf.len() >= HEADER && be_u64(&buf[16..]) == 0
}

pub(crate) fn on_file_chunk(buf: &[u8]) -> PgResult<()> {
    if buf.len() < HEADER {
        return violation(format!("{} bytes", buf.len()));
    }
    let (lsn, len, off) = (be_u64(&buf[0..]), be_u64(&buf[8..]), be_u64(&buf[16..]));
    let bytes = &buf[HEADER..];
    let mut cur = RECV.with(|r| r.borrow_mut().take());
    if off == 0 {
        // A new file (a resend after a reconnect starts over: the stale .recv is truncated).
        match Spool::data().land_begin(lsn) {
            Ok(landing) => cur = Some(Receiving { landing, len, next: 0 }),
            Err(e) => return io_error(e, lsn),
        }
    }
    let Some(mut rc) = cur else {
        return violation(format!("file {} chunk at offset {off} before its start", pgrc_format_name(lsn)));
    };
    if rc.landing.lsn() != lsn || rc.len != len || rc.next != off || off + bytes.len() as u64 > len {
        return violation(format!(
            "file {} chunk at offset {off} ({} bytes of {len}); expected file {} at offset {} of {}",
            pgrc_format_name(lsn),
            bytes.len(),
            pgrc_format_name(rc.landing.lsn()),
            rc.next,
            rc.len
        ));
    }
    if let Err(e) = rc.landing.write_at(off, bytes) {
        return io_error(e, lsn);
    }
    rc.next += bytes.len() as u64;
    if rc.next < len {
        RECV.with(|r| *r.borrow_mut() = Some(rc));
        return injection_point::injection_point("pgrc-recv-mid-file");
    }
    if let Err(e) = rc.landing.finish() {
        return io_error(e, lsn);
    }
    transam_xlog::spool_grew(lsn)?; // WR L4b: landed bytes count toward the restartpoint distance
    let _ = ereport(DEBUG1)
        .errmsg_internal(format!("pgrc walreceiver: landed file {} ({len} bytes)", pgrc_format_name(lsn)))
        .finish(loc("pgrc_on_file_chunk"));
    Ok(())
}
