//! C6 (e): a limit user input reaches ends the statement with PostgreSQL's ERROR 54000 (program_limit_exceeded), raised
//! through the one home `gravity_abi::limits::exceeded` as a `PgError` payload. A plain panic aborts the server since
//! D2933 (tcop's main_loop turns only a `PgError` into an ERROR); before C6 (e) these limits were plain asserts.
use gravity_abi::breaker::{SortRun, TopK};

fn limit_error(body: impl FnOnce()) -> types_error::PgError {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).expect_err("the limit raises");
    match payload.downcast::<types_error::PgError>() {
        Ok(e) => *e,
        Err(p) => panic!("a plain panic, which aborts the server: {:?}", p.downcast_ref::<String>().map(String::as_str).or(p.downcast_ref::<&str>().copied())),
    }
}

#[test]
fn a_share_too_small_for_one_sort_row_raises_54000() {
    let mut region = vec![0u64; 64];
    let e = limit_error(|| unsafe {
        SortRun::rearm(region.as_mut_ptr() as *mut u8, 64, 512);
    });
    assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert!(e.message().starts_with("gravity: a share too small for one sort row: 64 bytes"), "{}", e.message());
}

#[test]
fn a_top_k_run_below_its_limit_raises_54000() {
    let mut region = vec![0u64; 512];
    let e = limit_error(|| unsafe {
        TopK::at_cap(region.as_mut_ptr() as *mut u8, 5, 2, 1, 0);
    });
    assert_eq!(e.sqlstate(), types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
    assert!(e.message().starts_with("gravity: top-k run capacity: 2 rows for a LIMIT of 5"), "{}", e.message());
}
