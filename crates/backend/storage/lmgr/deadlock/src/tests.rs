use super::*;

#[test]
fn workspace_preallocates_c_sizes_and_seams_install() {
    init_seams();
    init_small::globals::SetMaxBackends(16);
    deadlock_seams::init_dead_lock_checking::call().unwrap();
    WORKSPACE.with(|w| {
        let w = w.borrow();
        let ws = w.as_ref().unwrap();
        assert!(ws.visitedProcs.len() >= 16);
        assert!(ws.waitOrders.len() >= 8);
        assert!(ws.possibleConstraints.len() >= 64);
        assert_eq!(ws.maxCurConstraints, 16);
    });
    assert!(deadlock_seams::dead_lock_check::is_installed());
    assert!(deadlock_seams::dead_lock_report::is_installed());
    assert!(deadlock_seams::remember_simple_deadlock::is_installed());
    assert!(deadlock_seams::get_blocking_autovacuum_procno::is_installed());
}

#[test]
fn workspace_uses_top_memory_context() {
    let top = init_small::globals::TopMemoryContext();
    let before = top.stats().used;
    init_small::globals::SetMaxBackends(16);
    InitDeadLockChecking().unwrap();
    assert!(std::ptr::eq(backend_mcx().context(), top));
    assert!(top.stats().used > before);
}

#[test]
fn deadlock_report_preserves_c_error_location() {
    init_small::globals::SetMaxBackends(16);
    InitDeadLockChecking().unwrap();
    let track_counts = pgstat::pgstat_track_counts();
    pgstat::set_pgstat_track_counts(false);
    let err = DeadLockReport().unwrap_err();
    pgstat::set_pgstat_track_counts(track_counts);
    let location = err.location().unwrap();
    assert_eq!(location.filename.as_deref(), Some("deadlock.c"));
    assert_eq!(location.lineno, 1138);
    assert_eq!(location.funcname.as_deref(), Some("DeadLockReport"));
}
