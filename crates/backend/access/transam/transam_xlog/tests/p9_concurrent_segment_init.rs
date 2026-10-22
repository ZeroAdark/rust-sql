// P9: every backend and auxiliary process is a thread of one OS process, so a
// WAL temp file named after the pid alone (C's pg_wal/xlogtemp.<getpid()>) is
// shared by every thread. Two threads creating a segment at once unlinked,
// overwrote or renamed each other's temp file: "could not open file
// pg_wal/xlogtemp.<pid>" at LOG, then an ERROR on the segment the loser
// reopens (seen live from a backend's segment switch against the
// checkpointer's preallocation, and from a standby's walreceiver).
//
// Several threads with a PGPROC each (and one shared MyProcPid, as in a test
// harness or a thread without its own pid) create distinct segments at the
// same moment, round after round. Every creation must succeed, every segment
// must be installed at full size, and no temp file may be left behind.
//
// NATIVE ARM ONLY: std::fs fixture plumbing + the fd data plane.
#![cfg(not(pgrust_sim))]

use std::sync::{Arc, Barrier};

const THREADS: usize = 8;
const ROUNDS: u64 = 6;
// Far past the boot's END_OF_LOG (segment 2): never created by the fixture.
const FIRST_SEGNO: u64 = 100;
const TAG: &str = "p9_xlogtemp";

fn temps(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.join("pg_wal"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("xlogtemp."))
        .collect();
    v.sort();
    v
}

#[test]
fn concurrent_segment_creation_never_collides() {
    // durable_rename's fsync_fname opens the temp file as a transient file,
    // which records the creating subtransaction.
    if !xact_seams::get_current_sub_transaction_id::is_installed() {
        xact_seams::get_current_sub_transaction_id::set(|| 1);
    }
    test_boot::boot_wal(TAG);
    let dir = test_boot::data_dir().clone();
    let segsz = transam_xlog::wal_segment_size() as u64;
    assert!(guc_tables::vars::wal_init_zero.read(), "the zero-fill is the collision window");

    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let workers: Vec<_> = (0..THREADS as u64)
        .map(|t| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                test_boot::boot_wal(TAG);
                let mut out = Vec::new();
                for round in 0..ROUNDS {
                    let segno = FIRST_SEGNO + round * THREADS as u64 + t;
                    barrier.wait();
                    // A panic (e.g. a debug assertion on a stolen temp file)
                    // is a failure of this creation, not a lost thread: every
                    // thread must reach the next barrier.
                    let r = std::panic::catch_unwind(|| transam_xlog::write::XLogFileInit(segno, 1));
                    let r = match r {
                        Ok(Ok(fd)) => {
                            // SAFETY: XLogFileInit hands back an open fd we own.
                            unsafe { libc::close(fd) };
                            Ok(())
                        }
                        Ok(Err(e)) => Err(e.message().to_string()),
                        Err(p) => Err(format!(
                            "panic: {}",
                            p.downcast_ref::<String>().map(String::as_str).or(p.downcast_ref::<&str>().copied()).unwrap_or("?")
                        )),
                    };
                    out.push((segno, r));
                    barrier.wait();
                }
                out
            })
        })
        .collect();

    let mut leftovers = Vec::new();
    for _ in 0..ROUNDS {
        barrier.wait(); // start the round
        barrier.wait(); // every thread's creation returned
        leftovers.extend(temps(&dir));
    }
    let mut failures = Vec::new();
    for w in workers {
        for (segno, r) in w.join().unwrap() {
            let path = dir.join(transam_xlog::XLogFilePath(1, segno, segsz as i32));
            match (r, std::fs::metadata(&path)) {
                (Ok(()), Ok(m)) if m.len() == segsz => {}
                (r, m) => failures.push(format!("segment {segno}: {r:?}, file {:?}", m.map(|m| m.len()))),
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} creations failed:\n{}", failures.len(), THREADS as u64 * ROUNDS, failures.join("\n"));
    assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
