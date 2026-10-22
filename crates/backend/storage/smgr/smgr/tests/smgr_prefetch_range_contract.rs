
use types_core::primitive::{ForkNumber, INVALID_PROC_NUMBER};
use types_core::BLCKSZ;
use types_storage::smgr::RELSEG_SIZE;
use types_storage::{RelFileLocator, RelFileLocatorBackend};

fn setup() -> std::path::PathBuf {
    guc_tables::init_seams();
    elog::init_seams();
    fd::init_seams();
    smgr::init_seams();

    xact_seams::get_current_sub_transaction_id::set(|| 1);
    aio_seams::pgaio_closing_fd::set(|_| {});
    aio_seams::pgaio_io_start_readv::set(|_, _, _| Ok(()));
    waitevent_seams::pgstat_report_wait_start::set(|_| {});
    waitevent_seams::pgstat_report_wait_end::set(|| {});
    pgstat_seams::pgstat_report_tempfile::set(|_| {});
    relpath_seams::relpathbackend::set(|rlocator, backend, forknum| {
        assert_eq!(forknum, ForkNumber::MAIN_FORKNUM);
        if backend == INVALID_PROC_NUMBER {
            format!("base/{}/{}", rlocator.dbOid, rlocator.relNumber)
        } else {
            format!("base/{}/t{}_{}", rlocator.dbOid, backend, rlocator.relNumber)
        }
    });
    sync_seams::register_sync_request::set(|_tag, _ty, _retry| Ok(true));
    tablespace_seams::tablespace_create_dbspace::set(|_, _, _| Ok(()));

    let dir = std::env::temp_dir().join(format!("pgrust_smgr_prefetch_range_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("base/5")).unwrap();
    std::env::set_current_dir(&dir).unwrap();
    fd::InitFileAccess();
    dir
}

fn key(rel: u32, backend: i32) -> RelFileLocatorBackend {
    RelFileLocatorBackend { locator: RelFileLocator { spcOid: 1663, dbOid: 5, relNumber: rel }, backend }
}

fn create(k: RelFileLocatorBackend, nblocks: u32) {
    let fork = ForkNumber::MAIN_FORKNUM;
    smgr::smgropen(k.locator, k.backend).unwrap();
    smgr::smgrcreate(k, fork, false).unwrap();
    let block = [0x5au8; BLCKSZ];
    for b in 0..nblocks {
        smgr::smgrextend(k, fork, b, &block, false).unwrap();
    }
}

#[test]
fn prefetch_range_seam_md_contract() {
    let dir = setup();
    let main = ForkNumber::MAIN_FORKNUM;
    let shared = |rel| key(rel, INVALID_PROC_NUMBER);

    create(shared(16390), 40);
    assert_eq!(smgr_seams::smgr_nblocks::call(shared(16390), main).unwrap(), 40);
    assert!(smgr_seams::smgr_prefetch::call(shared(16390), main, 0, 16).unwrap());
    assert!(smgr_seams::smgr_prefetch::call(shared(16390), main, 32, 8).unwrap());
    let past = smgr_seams::smgr_prefetch::call(shared(16390), main, RELSEG_SIZE, 16);
    assert!(past.is_err(), "md raises for a missing segment: {past:?}");

    create(shared(16391), 4);
    smgr::AtEOXact_SMgr().unwrap();
    std::fs::remove_file(dir.join("base/5/16391")).unwrap();
    let e = smgr_seams::smgr_nblocks::call(shared(16391), main).unwrap_err();
    assert_eq!(e.sqlstate, types_error::ERRCODE_UNDEFINED_FILE, "dropped: {}", e.message);

    create(key(16392, 3), 4);
    assert_eq!(smgr_seams::smgr_nblocks::call(key(16392, 3), main).unwrap(), 4);
    let e = smgr_seams::smgr_nblocks::call(shared(16392), main).unwrap_err();
    assert_eq!(e.sqlstate, types_error::ERRCODE_UNDEFINED_FILE, "temp at the shared path: {}", e.message);

    let _ = std::fs::remove_dir_all(dir);
}
