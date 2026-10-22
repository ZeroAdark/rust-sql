// D3649 witness: a parallel-worker / parked-task GUC bind (bind_session_gucs,
// or restore_guc_state on the string-capture path; RestoreGUCState's port)
// must never publish a boot value over a process-wide backing that the
// leader's capture re-applies. autovacuum is SIGHUP, so its backing
// (AV_START_DAEMON in the server) is one process-wide cell: with autovacuum =
// off in the config, the bind's reset-to-boot pass used to write true there
// until the capture wrote off again, and an index build in another session
// read AutoVacuumingActive() = true in that window and wrote the heap's
// reltuples / relpages. In C both steps touch only the worker's own address
// space, so no other backend can see the transient.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use guc_tables::GucVarAccessors;
use types_guc::{GucContext, GucSource};

static AUTOVACUUM: AtomicBool = AtomicBool::new(true);
static WRITES: Mutex<Vec<bool>> = Mutex::new(Vec::new());

fn autovacuum_get() -> bool {
    AUTOVACUUM.load(Ordering::Relaxed)
}

fn autovacuum_set(v: bool) {
    WRITES.lock().unwrap().push(v);
    AUTOVACUUM.store(v, Ordering::Relaxed);
}

fn setup_seams() {
    guc_tables::init_seams();
    elog::init_seams();
    guc::init_seams();
    xact_seams::is_in_parallel_mode::set(|| false);
    scalar_seams::parse_bool::set(|value| match value {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    });
    aclchk_seams::pg_parameter_aclcheck_set::set(|_, _| Ok(true));
    mbutils_seams::get_database_encoding::set(|| 6);
    timestamp_seams::get_current_timestamp::set(|| 0);
    guc_tables::vars::autovacuum_start_daemon.install(GucVarAccessors {
        get: autovacuum_get,
        set: autovacuum_set,
    });
}

// A worker thread brought up from the postmaster snapshot (so it holds the
// config's autovacuum at PGC_S_FILE, as a pooled worker does), then bound to
// `caps`; returns the backing writes the bind made.
fn bind_on_worker(
    snapshot: Vec<guc::store::NondefaultGuc>,
    caps: Vec<guc::store::CapturedGuc>,
) -> Vec<bool> {
    std::thread::spawn(move || {
        miscinit::SetUserIdAndSecContext(10, 0);
        guc::store::initialize_guc_options_for_child(&snapshot).unwrap();
        guc::store::restore_nondefault_variables(&snapshot).unwrap();
        assert_eq!(guc::store::get_bool("autovacuum"), Some(false));
        WRITES.lock().unwrap().clear();
        let binding = guc::store::bind_session_gucs(&caps).unwrap();
        let writes = WRITES.lock().unwrap().clone();
        drop(binding);
        writes
    })
    .join()
    .unwrap()
}

#[test]
fn worker_bind_never_publishes_boot_value_the_capture_reapplies() {
    setup_seams();
    guc::store::initialize_guc_options().unwrap();

    // The leader before the config is read: autovacuum at its default.
    let caps_default = guc::store::capture_session_gucs();
    assert!(!caps_default.iter().any(|c| c.name() == "autovacuum"));

    guc::SetConfigOption(
        "autovacuum",
        Some("off"),
        GucContext::PGC_SIGHUP,
        GucSource::PGC_S_FILE,
    )
    .unwrap();
    assert!(!autovacuum_get());
    // One postmaster snapshot per worker bring-up (NondefaultGuc is not Clone).
    let snapshot = guc::store::capture_nondefault_variables;
    assert!(snapshot().iter().any(|v| v.name == "autovacuum"));
    let caps_off = guc::store::capture_session_gucs();
    assert!(caps_off.iter().any(|c| c.name() == "autovacuum"));

    // (1) The leader carries autovacuum = off: the bind's reset to boot must
    // not write true over the process-wide backing before the capture's off.
    let writes = bind_on_worker(snapshot(), caps_off);
    assert!(
        !writes.contains(&true),
        "worker bind published autovacuum's boot value (on) process-wide: writes {writes:?}"
    );
    assert!(!autovacuum_get(), "autovacuum must end off, as the leader has it");

    // (2) The leader holds autovacuum at its default (it has re-read a config
    // without the line): the worker's reset to boot is the final value, and
    // its backing must say so.
    let writes = bind_on_worker(snapshot(), caps_default);
    assert_eq!(writes, vec![true], "the worker's boot value must still be published");
    assert!(autovacuum_get());

    // (3) The string-capture path (PGRUST_NO_GUC_BIND): the same reset, then
    // restore_nondefault_variables of the leader's snapshot (autovacuum off).
    autovacuum_set(false);
    let (worker, leader) = (snapshot(), snapshot());
    let writes = std::thread::spawn(move || {
        miscinit::SetUserIdAndSecContext(10, 0);
        guc::store::initialize_guc_options_for_child(&worker).unwrap();
        guc::store::restore_nondefault_variables(&worker).unwrap();
        WRITES.lock().unwrap().clear();
        guc::store::restore_guc_state(&leader).unwrap();
        WRITES.lock().unwrap().clone()
    })
    .join()
    .unwrap();
    assert!(
        !writes.contains(&true),
        "string-path restore published autovacuum's boot value (on): writes {writes:?}"
    );
    assert!(!autovacuum_get());
}
