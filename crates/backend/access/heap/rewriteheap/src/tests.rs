// Wait-event witnesses for the logical-rewrite mapping file I/O
// (audit-18.6 w2-034, a186-candidate-fp-heap-rewriteheap-e01e8c21d9efbd22f6c5-1):
// rewriteheap.c reports WAIT_EVENT_LOGICAL_REWRITE_WRITE (FileWrite, :881),
// WAIT_EVENT_LOGICAL_REWRITE_SYNC (FileSync, :922) and
// WAIT_EVENT_LOGICAL_REWRITE_CHECKPOINT_SYNC (pg_fsync, :1236) around the
// three I/O calls. Ids are the positions in waitevent's IO name table
// (wait_event_names.txt order).
use std::sync::{Mutex, MutexGuard, Once};

use super::*;

const PG_WAIT_IO: u32 = 0x0A00_0000;
const WAIT_EVENT_LOGICAL_REWRITE_CHECKPOINT_SYNC: u32 = PG_WAIT_IO | 34;
const WAIT_EVENT_LOGICAL_REWRITE_SYNC: u32 = PG_WAIT_IO | 37;
const WAIT_EVENT_LOGICAL_REWRITE_WRITE: u32 = PG_WAIT_IO | 39;

// Every pgstat_report_wait_start(info) / pgstat_report_wait_end() (recorded
// as 0) the crate issued through the waitevent seam, in order.
static WAIT_EVENTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn setup() {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        waitevent_seams::pgstat_report_wait_start::set(|info| {
            WAIT_EVENTS.lock().unwrap().push(info);
        });
        waitevent_seams::pgstat_report_wait_end::set(|| {
            WAIT_EVENTS.lock().unwrap().push(0);
        });
        guc_tables::vars::temp_file_limit
            .install_if_absent(guc_tables::GucVarAccessors { get: || -1, set: |_| {} });
    });
    fd::vfd::InitFileAccess();
    WAIT_EVENTS.lock().unwrap().clear();
}

fn take_events() -> Vec<u32> {
    core::mem::take(&mut *WAIT_EVENTS.lock().unwrap())
}

/// A test's temp file or directory, removed when dropped (the test's end, or a panic's unwind): the suite leaves no
/// rewriteheap-* in the temp dir.
struct TempGone(std::path::PathBuf);

impl Drop for TempGone {
    fn drop(&mut self) {
        let _ = if self.0.is_dir() { std::fs::remove_dir_all(&self.0) } else { std::fs::remove_file(&self.0) };
    }
}

/// A mapping file in this process's scratch directory, and the guard that removes the directory.
fn scratch_mapping_file(name: &str) -> (RewriteMappingFile, TempGone) {
    let dir = std::env::temp_dir().join(format!("rewriteheap-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    (open_mapping_file(path).unwrap(), TempGone(dir))
}

#[test]
fn scratch_files_leave_nothing_behind() {
    let _s = serial();
    setup();
    let (src, gone) = scratch_mapping_file("map-gone");
    let dir = gone.0.clone();
    drop(src);
    drop(gone);
    assert!(!dir.exists(), "{} left behind", dir.display());
    let file = TempGone(std::env::temp_dir().join(format!("rewriteheap-gone-{}", std::process::id())));
    std::fs::write(&file.0, b"x").unwrap();
    let path = file.0.clone();
    drop(file);
    assert!(!path.exists(), "{} left behind", path.display());
}

// rewriteheap.c:880-881: FileWrite(..., WAIT_EVENT_LOGICAL_REWRITE_WRITE).
#[test]
fn mapping_file_write_reports_logical_rewrite_write() {
    let _s = serial();
    setup();
    let (mut src, _gone) = scratch_mapping_file("map-write");
    let waldata = [0xA5u8; 2 * LOGICAL_REWRITE_MAPPING_SIZE];
    logical_mapping_file_write(&mut src, &waldata).unwrap();
    assert_eq!(src.off, waldata.len() as u64, "src->off advanced by the bytes written");
    assert_eq!(std::fs::read(&src.path).unwrap(), waldata.to_vec());
    assert_eq!(
        take_events(),
        vec![WAIT_EVENT_LOGICAL_REWRITE_WRITE, 0],
        "the mapping write is bracketed by LogicalRewriteWrite"
    );
    // A second batch lands at the advanced offset, under its own bracket.
    logical_mapping_file_write(&mut src, &waldata).unwrap();
    assert_eq!(src.off, 2 * waldata.len() as u64);
    assert_eq!(take_events(), vec![WAIT_EVENT_LOGICAL_REWRITE_WRITE, 0]);
}

// rewriteheap.c:921-922: FileSync(src->vfd, WAIT_EVENT_LOGICAL_REWRITE_SYNC).
#[test]
fn mapping_file_sync_reports_logical_rewrite_sync() {
    let _s = serial();
    setup();
    let (mut src, _gone) = scratch_mapping_file("map-sync");
    logical_mapping_file_write(&mut src, &[1u8; LOGICAL_REWRITE_MAPPING_SIZE]).unwrap();
    take_events();
    logical_mapping_file_sync(&src).unwrap();
    assert_eq!(
        take_events(),
        vec![WAIT_EVENT_LOGICAL_REWRITE_SYNC, 0],
        "the end-of-rewrite fsync is bracketed by LogicalRewriteSync"
    );
}

// rewriteheap.c:921 FileSync and :1236 pg_fsync both go through fd.c
// pg_fsync, which returns success without touching the descriptor when
// fsync=off (fd.c:445); a socket, which no fsync(2) accepts, proves the
// syscall was skipped.
#[test]
fn mapping_file_syncs_skip_the_syscall_when_fsync_is_off() {
    let _s = serial();
    setup();
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    let (sock, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    // SAFETY: the descriptor's only owner from here on is `file`.
    let file = unsafe { std::fs::File::from_raw_fd(sock.into_raw_fd()) };
    let path = std::path::PathBuf::from("pg_logical/mappings/map-pipe");
    let (src, _gone) = scratch_mapping_file("map-nosync");
    let was = init_small::globals::enableFsync();
    init_small::globals::set_enableFsync(false);
    let synced = logical_mapping_file_sync(&src);
    let ckpt = checkpoint_fsync_mapping_file(&file, &path);
    init_small::globals::set_enableFsync(was);
    synced.unwrap();
    ckpt.unwrap();
    assert_eq!(
        take_events(),
        vec![
            WAIT_EVENT_LOGICAL_REWRITE_SYNC,
            0,
            WAIT_EVENT_LOGICAL_REWRITE_CHECKPOINT_SYNC,
            0
        ],
        "the wait events are still reported around the skipped fsync"
    );
}

// rewriteheap.c:1210/1224/1237/1240: every report names the datadir-relative
// pg_logical/mappings/<file>, never the absolute path the I/O used, and
// carries strerror through %m (batch-232 fp-heap-rewriteheap#6/#7).
#[test]
fn checkpoint_reports_name_the_datadir_relative_mapping_file() {
    let _s = serial();
    setup();
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    let (sock, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    // SAFETY: the descriptor's only owner from here on is `file`.
    let file = unsafe { std::fs::File::from_raw_fd(sock.into_raw_fd()) };
    let path = std::env::temp_dir()
        .join("b232-datadir")
        .join("pg_logical/mappings/map-1-2-0_3-4-5");
    let was = init_small::globals::enableFsync();
    init_small::globals::set_enableFsync(true);
    // data_sync_retry=on keeps the fsync failure at ERROR (fd.c
    // data_sync_elevel) so the report is inspectable here.
    fd::vfd::set_data_sync_retry(true);
    let err = checkpoint_fsync_mapping_file(&file, &path).unwrap_err();
    fd::vfd::set_data_sync_retry(false);
    init_small::globals::set_enableFsync(was);
    let msg = err.message().to_string();
    assert!(
        msg.starts_with("could not fsync file \"pg_logical/mappings/map-1-2-0_3-4-5\": "),
        "{msg}"
    );
    assert!(!msg.contains("os error"), "{msg}");
    assert!(!msg.contains("b232-datadir"), "{msg}");
}

// rewriteheap.c:1236-1240: pgstat_report_wait_start(
// WAIT_EVENT_LOGICAL_REWRITE_CHECKPOINT_SYNC) around the checkpoint's
// pg_fsync of a surviving mapping file.
#[test]
fn checkpoint_fsync_reports_logical_rewrite_checkpoint_sync() {
    let _s = serial();
    setup();
    let path = std::env::temp_dir().join(format!("rewriteheap-ckpt-{}", std::process::id()));
    let _gone = TempGone(path.clone());
    let file = std::fs::File::create(&path).unwrap();
    checkpoint_fsync_mapping_file(&file, &path).unwrap();
    assert_eq!(
        take_events(),
        vec![WAIT_EVENT_LOGICAL_REWRITE_CHECKPOINT_SYNC, 0],
        "the checkpoint fsync is bracketed by LogicalRewriteCheckpointSync"
    );
}

// rewriteheap.c:963 PathNameOpenFile: VFDs survive more xids than fds.
#[test]
fn mapping_files_are_vfds_not_raw_descriptors() {
    let _s = serial();
    setup();
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain getrlimit/setrlimit on this process's RLIMIT_NOFILE.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    let low = libc::rlimit { rlim_cur: 256, rlim_max: lim.rlim_max };
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
    let _gone = TempGone(std::env::temp_dir().join(format!("rewriteheap-many-{}", std::process::id())));
    let opened: Vec<PgResult<RewriteMappingFile>> = (0..400)
        .map(|i| {
            let dir = std::env::temp_dir().join(format!("rewriteheap-many-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("map-{i}"));
            let _ = std::fs::remove_file(&path);
            let mut src = open_mapping_file(path)?;
            logical_mapping_file_write(&mut src, &[i as u8; LOGICAL_REWRITE_MAPPING_SIZE])?;
            Ok(src)
        })
        .collect();
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    for r in &opened {
        assert!(r.is_ok(), "mapping file open/write failed");
    }
    take_events();
}

// fd.c FileWriteV: a short mapping write reports ENOSPC, not a stale errno.
//
// RLIMIT_FSIZE is the whole process's: lowered here, it also fails every other
// write to a regular file while it holds, the harness's own output included
// when that is redirected to a file (its main thread dies, and the run fails
// with a SendError and leaves every running test's temp files). So the test
// runs alone in a child of this binary, its output a pipe.
#[test]
fn mapping_short_write_reports_no_space() {
    const CHILD: &str = "PGRUST_REWRITEHEAP_FSIZE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::mapping_short_write_reports_no_space"])
            .args(["--test-threads", "1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "child failed: {text}");
        assert!(text.contains("1 passed"), "the child ran no test: {text}");
        return;
    }
    let _s = serial();
    setup();
    let path = std::env::temp_dir().join(format!("rewriteheap-short-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _gone = TempGone(path.clone());
    let mut src = open_mapping_file(path.clone()).unwrap();
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: SIGXFSZ disposition and RLIMIT_FSIZE of this test process.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        libc::getrlimit(libc::RLIMIT_FSIZE, &mut lim);
        let low = libc::rlimit { rlim_cur: 4, rlim_max: lim.rlim_max };
        libc::setrlimit(libc::RLIMIT_FSIZE, &low);
    }
    let r = logical_mapping_file_write(&mut src, &[0u8; LOGICAL_REWRITE_MAPPING_SIZE]);
    unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &lim) };
    drop(src);
    let _ = std::fs::remove_file(&path);
    let msg = r.unwrap_err().message().to_string();
    assert!(msg.contains("wrote 4 of") && msg.contains("No space left on device"), "{msg}");
    take_events();
}
