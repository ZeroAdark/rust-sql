use std::sync::Once;

use types_core::{ForkNumber, BLCKSZ, INVALID_PROC_NUMBER};
use types_storage::sync::{FileTag, SyncRequestHandler, SyncRequestType};
use types_storage::{RelFileLocator, RelFileLocatorBackend};

use super::*;

fn fork_suffix(forknum: ForkNumber) -> &'static str {
    match forknum {
        ForkNumber::MAIN_FORKNUM => "",
        ForkNumber::FSM_FORKNUM => "_fsm",
        ForkNumber::VISIBILITYMAP_FORKNUM => "_vm",
        ForkNumber::INIT_FORKNUM => "_init",
        ForkNumber::InvalidForkNumber => panic!("invalid fork"),
    }
}

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        guc_tables::init_seams();
        elog::init_seams();
        fd::init_seams();
        smgr::init_seams();
        crate::init_seams();

        xact_seams::get_current_sub_transaction_id::set(|| 1);
        aio_seams::pgaio_closing_fd::set(|_| {});
        aio_seams::pgaio_io_start_readv::set(|_, _, _| Ok(()));
        waitevent_seams::pgstat_report_wait_start::set(|_| {});
        waitevent_seams::pgstat_report_wait_end::set(|| {});
        pgstat_seams::pgstat_report_tempfile::set(|_| {});
        relpath_seams::relpathbackend::set(|rlocator, _backend, forknum| {
            format!(
                "base/{}/{}{}",
                rlocator.dbOid,
                rlocator.relNumber,
                fork_suffix(forknum)
            )
        });
        relpath_seams::relpathperm::set(|rlocator, forknum| {
            format!(
                "base/{}/{}{}",
                rlocator.dbOid,
                rlocator.relNumber,
                fork_suffix(forknum)
            )
        });
        tablespace_seams::tablespace_create_dbspace::set(|_, _, _| Ok(()));
        // fdatasync (PLATFORM_DEFAULT_WAL_SYNC_METHOD on macOS): fd's
        // writethrough branch stays cold.
        guc_tables::vars::wal_sync_method.install(guc_tables::GucVarAccessors {
            get: || 2,
            set: |_| {},
        });

        let dir = data_dir(std::process::id());
        std::fs::create_dir_all(dir.join("base/5")).unwrap();
        remove_data_dir_at_exit();
        std::env::set_current_dir(&dir).unwrap();
        println!("data dir {}", dir.display());
    });
    // Per test thread: VFD cache, pendingOps and GUC mirrors are all TLS.
    init_small::globals::set_enableFsync(true);
    fd::InitFileAccess();
    InitSync().unwrap();
}

fn data_dir(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pgrust_sync_{pid}"))
}

/// The process's data directory (every test's working directory, so no
/// test's end may remove it) goes at the process's exit.
fn remove_data_dir_at_exit() {
    extern "C" fn gone() {
        let _ = std::fs::remove_dir_all(data_dir(std::process::id()));
    }
    extern "C" {
        fn atexit(f: extern "C" fn()) -> core::ffi::c_int;
    }
    // SAFETY: the C library's atexit, given a function that cannot unwind
    // (it ignores the removal's error).
    unsafe { atexit(gone) };
}

/// A run of this test alone, in a child process, leaves no data directory
/// behind once the child exits.
#[test]
fn a_run_leaves_nothing_behind() {
    const CHILD: &str = "PGRUST_SYNC_LEAVE_NOTHING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        setup();
        create_rel_with_block(20099);
        return;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::a_run_leaves_nothing_behind", "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let dir = data_dir(child.id());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed: {stdout}");
    assert!(
        stdout.contains(&format!("data dir {}", dir.display())),
        "the child never made its data dir: {stdout}"
    );
    assert!(!dir.exists(), "{} left behind", dir.display());
}

fn key(rel: u32) -> RelFileLocatorBackend {
    RelFileLocatorBackend {
        locator: RelFileLocator { spcOid: 1663, dbOid: 5, relNumber: rel },
        backend: INVALID_PROC_NUMBER,
    }
}

fn create_rel_with_block(rel: u32) -> RelFileLocatorBackend {
    let k = key(rel);
    smgr::smgropen(k.locator, k.backend).unwrap();
    smgr::smgrcreate(k, ForkNumber::MAIN_FORKNUM, false).unwrap();
    let block = [0x5Au8; BLCKSZ];
    smgr::smgrextend(k, ForkNumber::MAIN_FORKNUM, 0, &block, false).unwrap();
    k
}

fn md_tag(rel: u32) -> FileTag {
    FileTag::new(
        SyncRequestHandler::SYNC_HANDLER_MD,
        ForkNumber::MAIN_FORKNUM,
        key(rel).locator,
        0,
    )
}

#[test]
fn extend_registers_and_process_drains() {
    setup();
    create_rel_with_block(20001);
    let (fsyncs, _) = pending_counts();
    assert!(fsyncs >= 1, "smgrextend registered a dirty segment");
    ProcessSyncRequests().unwrap();
    assert_eq!(pending_counts().0, 0);
}

#[test]
fn duplicate_requests_merge() {
    setup();
    let tag = md_tag(20002);
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    assert_eq!(pending_counts().0, 1);
    RegisterSyncRequest(tag, SyncRequestType::SYNC_FORGET_REQUEST, true).unwrap();
    // Canceled entry: removed without touching the (nonexistent) file.
    ProcessSyncRequests().unwrap();
    assert_eq!(pending_counts().0, 0);
}

// sync.c RememberSyncRequest: a duplicate SYNC_REQUEST is HASH_ENTER on a
// present key, which never grows (or fails to grow) the table.
#[test]
fn duplicate_request_does_not_grow_pending_ops() {
    setup();
    let tag = md_tag(20010);
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    let mut rel = 20011;
    while pending_counts().0 < pending_ops_capacity() {
        RegisterSyncRequest(md_tag(rel), SyncRequestType::SYNC_REQUEST, false).unwrap();
        rel += 1;
    }
    let cap = pending_ops_capacity();
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    assert_eq!(pending_ops_capacity(), cap);
}

#[test]
fn missing_file_fails_after_retry() {
    setup();
    // data_sync_retry=on keeps fsync failure at ERROR; default promotes to
    // PANIC (process abort, covered by fsync_failure_aborts_process).
    fd::vfd::set_data_sync_retry(true);
    let tag = md_tag(20003);
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    let err = ProcessSyncRequests().unwrap_err();
    assert!(
        format!("{err:?}").contains("could not fsync file"),
        "got: {err:?}"
    );
}

// sync.c:441-453 (ProcessSyncRequests): the first ENOENT on a file is
// reported at DEBUG1 with errmsg_internal("could not fsync file \"%s\" but
// retrying: %m") before the absorb-and-retry; only the second failure is the
// data_sync_elevel(ERROR) report.
#[test]
fn missing_file_logs_debug1_retry_notice() {
    setup();
    fd::vfd::set_data_sync_retry(true);

    static SEEN: std::sync::Mutex<Vec<(elog::ErrorLevel, String)>> = std::sync::Mutex::new(Vec::new());
    fn hook(error: &elog::PgError, _output_to_server: &mut bool) {
        SEEN.lock().unwrap().push((error.level, error.message.clone()));
    }
    SEEN.lock().unwrap().clear();
    let prev_min = elog::config::log_min_messages();
    elog::config::set_log_min_messages(DEBUG1);
    let prev_hook = elog::set_emit_log_hook(Some(hook));

    let tag = md_tag(20009);
    RegisterSyncRequest(tag, SyncRequestType::SYNC_REQUEST, false).unwrap();
    let err = ProcessSyncRequests().unwrap_err();

    elog::set_emit_log_hook(prev_hook);
    elog::config::set_log_min_messages(prev_min);

    assert_eq!(
        err.message,
        "could not fsync file \"base/5/20009\": No such file or directory",
        "got: {err:?}"
    );
    let seen = SEEN.lock().unwrap();
    let retries: Vec<&(elog::ErrorLevel, String)> =
        seen.iter().filter(|(_, m)| m.contains("but retrying")).collect();
    assert_eq!(
        retries,
        [&(
            DEBUG1,
            "could not fsync file \"base/5/20009\" but retrying: No such file or directory".to_owned()
        )],
        "seen: {seen:?}"
    );
}

#[test]
fn filter_cancels_matching_database() {
    setup();
    RegisterSyncRequest(md_tag(20004), SyncRequestType::SYNC_REQUEST, false).unwrap();
    RegisterSyncRequest(md_tag(20005), SyncRequestType::SYNC_REQUEST, false).unwrap();
    // ForgetDatabaseSyncRequests' shape: dbOid-keyed FILTER cancels both.
    smgr::ForgetDatabaseSyncRequests(5).unwrap();
    ProcessSyncRequests().unwrap();
    assert_eq!(pending_counts().0, 0);
}

#[test]
fn filter_leaves_other_database() {
    setup();
    fd::vfd::set_data_sync_retry(true);
    RegisterSyncRequest(md_tag(20006), SyncRequestType::SYNC_REQUEST, false).unwrap();
    smgr::ForgetDatabaseSyncRequests(999).unwrap();
    assert!(ProcessSyncRequests().is_err(), "uncanceled missing file still fsyncs");
}

#[test]
fn unlink_waits_for_next_checkpoint_cycle() {
    setup();
    let k = create_rel_with_block(20007);
    ProcessSyncRequests().unwrap();
    let path = format!("base/{}/{}", k.locator.dbOid, k.locator.relNumber);
    assert!(std::path::Path::new(&path).exists());

    RegisterSyncRequest(md_tag(20007), SyncRequestType::SYNC_UNLINK_REQUEST, true).unwrap();
    // Same cycle: the entry is new, nothing is unlinked yet.
    SyncPostCheckpoint().unwrap();
    assert!(std::path::Path::new(&path).exists());
    assert_eq!(pending_counts().1, 1);

    SyncPreCheckpoint().unwrap();
    SyncPostCheckpoint().unwrap();
    assert!(!std::path::Path::new(&path).exists());
    assert_eq!(pending_counts().1, 0);
}

#[test]
#[ignore = "abort child of fsync_failure_aborts_process"]
fn fsync_failure_abort_child() {
    setup();
    RegisterSyncRequest(md_tag(20009), SyncRequestType::SYNC_REQUEST, false).unwrap();
    // data_sync_retry=off: the fsync-failure ereport must not return control
    // (fsyncgate); elog PANIC unwinds PanicExitThread under the thread model
    // (notes/crash-restart-design.md) and the postmaster maps it to
    // WTERMSIG(SIGABRT).
    let r = std::panic::catch_unwind(|| ProcessSyncRequests());
    match r {
        Err(payload) if payload.is::<types_error::PanicExitThread>() => std::process::exit(42),
        _ => std::process::exit(0),
    }
}

#[test]
fn fsync_failure_aborts_process() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["tests::fsync_failure_abort_child", "--exact", "--ignored", "--test-threads=1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(42), "child must unwind PanicExitThread: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("PANIC:  could not fsync file"), "missing PANIC report: {stderr}");
}

fn pgrc_tag(db: u32, rel: u32) -> FileTag {
    let locator = RelFileLocator { spcOid: 1663, dbOid: db, relNumber: rel };
    FileTag::new(SyncRequestHandler::SYNC_HANDLER_PGRC, ForkNumber::MAIN_FORKNUM, locator, 0)
}

#[test]
fn pgrc_one_request_per_directory_syncs_its_names() {
    setup();
    std::fs::create_dir_all("base/5/pgrc_20010").unwrap();
    std::fs::write("base/5/pgrc_20010/part-0.pgrc", b"pgrc").unwrap();
    let loc = pgrc_wal::dir_loc(key(20010).locator);
    pgrc_wal::sync::register(loc, ["part-0.pgrc".to_string(), "tmp-1-0.pgrct".to_string()]).unwrap();
    pgrc_wal::sync::register(loc, ["manifest-1.pgrcm".to_string()]).unwrap();
    assert_eq!(pending_counts().0, 1, "one request per directory");
    // The renamed-away tmp and the never-written manifest count as done.
    ProcessSyncRequests().unwrap();
    assert_eq!(pending_counts().0, 0);
}

#[test]
fn pgrc_filter_cancels_its_database_only() {
    setup();
    RegisterSyncRequest(pgrc_tag(5, 20011), SyncRequestType::SYNC_REQUEST, false).unwrap();
    RegisterSyncRequest(pgrc_tag(6, 20011), SyncRequestType::SYNC_REQUEST, false).unwrap();
    RegisterSyncRequest(pgrc_tag(5, 0), SyncRequestType::SYNC_FILTER_REQUEST, true).unwrap();
    let canceled = with_pending(|p| p.ops.iter().filter(|(_, e)| e.canceled).map(|(t, _)| t.rlocator.dbOid).collect::<Vec<_>>());
    assert_eq!(canceled, Some(vec![5]));
    ProcessSyncRequests().unwrap();
    assert_eq!(pending_counts().0, 0);
}
