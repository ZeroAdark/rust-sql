//! D1494: the run merge had no order cap. A region that could hold a buffer for every run merged them all at once
//! (fan = nruns): 650 runs at work_mem 1 MB held 650 run readers (and temporary files) open, where PostgreSQL's tuplesort
//! merges at most MAXORDER 500 tapes at once. `gravity_abi::limits::MAXORDER` now caps every fan; more runs merge in passes first.
use gravity_kernels::ops::sort::runmerge::{fan_in, fixed_bytes, var_emit_fan_in};
use gravity_abi::limits::SORT_VAR_MIN_BUF as VAR_MIN_BUF;

/// PostgreSQL tuplesort.c's MAXORDER (named literally so the test also runs where runmerge has no cap).
const PG_MAXORDER: usize = 500;

/// A region that could hold a reader for every run still merges at most PG_MAXORDER at once, as the emit's fan does; a
/// small region's fan is its own, as before.
#[test]
fn a_merge_reads_at_most_maxorder_runs_at_once() {
    let big = 1 << 30;
    assert_eq!(fan_in(big, VAR_MIN_BUF, VAR_MIN_BUF, 10_000), PG_MAXORDER);
    assert_eq!(fan_in(big, VAR_MIN_BUF, 0, 650), PG_MAXORDER, "650 runs at a large work_mem: passes, never 650 readers");
    assert_eq!(fan_in(big, VAR_MIN_BUF, 0, 300), 300, "fewer runs than the cap: every run in one merge");
    assert_eq!(var_emit_fan_in(big, 72, 650), PG_MAXORDER);
    assert_eq!(var_emit_fan_in(big, 72, PG_MAXORDER), PG_MAXORDER);
    let small = fixed_bytes(59) + 59 * VAR_MIN_BUF + VAR_MIN_BUF;
    assert_eq!(fan_in(small, VAR_MIN_BUF, VAR_MIN_BUF, 10_000), 59, "a 64 kB-class region keeps its own fan");
    assert_eq!(fan_in(big, 16, 16, 1), 1);
}
