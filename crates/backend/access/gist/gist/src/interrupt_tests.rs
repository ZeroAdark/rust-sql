//! Unit tests for check_for_interrupts (lib.rs): a pending interrupt routes
//! through the ported ProcessInterrupts seam (the spgist/gin/hash pattern)
//! instead of the former unported-panic stub. The same helper sits on the
//! scan (gistgettuple/gistgetbitmap), vacuum, and buffered-build paths.
use ::types_error::{PgError, ERRCODE_QUERY_CANCELED};

#[test]
fn pending_interrupt_routes_through_process_interrupts_seam() {
    // No pending interrupt: Ok without consulting the seam (which is not
    // installed yet at this point — a stub call would panic).
    init_small::globals::SetInterruptPending(false);
    assert!(crate::check_for_interrupts().is_ok());

    // ProcessInterrupts mock: consumes the flag and raises the cancel error,
    // as C's query-cancel arm does.
    ::postgres_seams::check_for_interrupts::set(|| {
        init_small::globals::SetInterruptPending(false);
        Err(Box::new(
            PgError::error("canceling statement due to user request")
                .with_sqlstate(ERRCODE_QUERY_CANCELED),
        ))
    });

    init_small::globals::SetInterruptPending(true);
    let err = crate::check_for_interrupts().unwrap_err();
    assert_eq!(err.message(), "canceling statement due to user request");
    assert_eq!(err.sqlstate(), ERRCODE_QUERY_CANCELED);

    // The interrupt was consumed on the seam side; the next check is clean.
    assert!(!init_small::globals::InterruptPending());
    assert!(crate::check_for_interrupts().is_ok());
}

// regression: the per-page delay points in gistvacuumscan and
// gistvacuum_delete_empty_pages reach vacuum_delay_point even while
// VacuumCostActive is false, so a pending autovacuum config reload is
// processed mid-scan (gistvacuum.c:248, :492).
static DELAY_POINT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn counting_delay_point(_is_analyze: bool) -> ::types_error::PgResult<()> {
    DELAY_POINT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

#[test]
fn vacuum_delay_point_reaches_the_seam_without_cost_active() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| vacuum_seams::vacuum_delay_point::set(counting_delay_point));
    assert!(!init_small::globals::VacuumCostActive());
    let before = DELAY_POINT_CALLS.load(std::sync::atomic::Ordering::SeqCst);
    crate::vacuum::vacuum_delay_point().unwrap();
    assert_eq!(
        DELAY_POINT_CALLS.load(std::sync::atomic::Ordering::SeqCst),
        before + 1
    );
}
