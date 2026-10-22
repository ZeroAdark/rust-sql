use super::*;

fn timed() -> Instrumentation {
    let mut i = Instrumentation::default();
    instr_init(&mut i, INSTRUMENT_TIMER);
    i
}

#[test]
fn init_sets_option_flags() {
    let mut i = Instrumentation::default();
    instr_init(&mut i, INSTRUMENT_TIMER | INSTRUMENT_BUFFERS);
    assert!(i.need_timer && i.need_bufusage && !i.need_walusage);
    instr_init(&mut i, 0);
    assert!(!i.need_timer && !i.need_bufusage);
}

#[test]
fn wal_option_arms_walusage() {
    let mut i = Instrumentation::default();
    instr_init(&mut i, INSTRUMENT_WAL);
    assert!(i.need_walusage);
    assert!(!i.need_timer && !i.need_bufusage);
}

#[test]
fn start_stop_end_loop_accumulates() {
    let mut i = timed();
    for _ in 0..3 {
        instr_start_node(&mut i);
        // The clock authority can be coarser than one call round-trip
        // (macOS CLOCK_MONOTONIC advances in ~1us steps) — wait for it to
        // tick so the accumulated interval is provably nonzero, exactly as
        // a C test over INSTR_TIME_SET_CURRENT would have to.
        let t0 = pg_clock::mono_ns();
        while pg_clock::mono_ns() == t0 {
            std::hint::spin_loop();
        }
        instr_stop_node(&mut i, 1.0);
    }
    assert!(i.running);
    assert_eq!(i.tuplecount, 3.0);
    assert!(i.counter.ticks > 0);
    assert!(i.firsttuple <= i.counter.get_double());

    instr_end_loop(&mut i);
    assert!(!i.running);
    assert_eq!(i.ntuples, 3.0);
    assert_eq!(i.nloops, 1.0);
    assert!(i.total >= i.startup);
    assert_eq!(i.tuplecount, 0.0);
    assert!(i.counter.is_zero());

    instr_start_node(&mut i);
    instr_stop_node(&mut i, 1.0);
    instr_end_loop(&mut i);
    assert_eq!(i.nloops, 2.0);
    assert_eq!(i.ntuples, 4.0);
}

#[test]
fn end_loop_without_activity_is_a_noop() {
    let mut i = timed();
    instr_end_loop(&mut i);
    assert_eq!(i.nloops, 0.0);
}

#[test]
#[should_panic(expected = "InstrStartNode called twice")]
fn double_start_matches_c_error() {
    let mut i = timed();
    instr_start_node(&mut i);
    instr_start_node(&mut i);
}

#[test]
#[should_panic(expected = "InstrStopNode called without start")]
fn stop_without_start_matches_c_error() {
    let mut i = timed();
    instr_stop_node(&mut i, 1.0);
}

#[test]
fn rows_only_mode_counts_without_clock() {
    let mut i = Instrumentation::default();
    instr_init(&mut i, 0);
    instr_start_node(&mut i);
    instr_stop_node(&mut i, 1.0);
    instr_start_node(&mut i);
    instr_stop_node(&mut i, 1.0);
    instr_end_loop(&mut i);
    assert_eq!(i.ntuples, 2.0);
    assert_eq!(i.nloops, 1.0);
    assert_eq!(i.total, 0.0);
    assert!(i.counter.is_zero());
}

#[test]
fn buffer_usage_diff_and_add_match_c() {
    let mut a = BufferUsage::default();
    let mut hi = BufferUsage::default();
    let lo = BufferUsage { shared_blks_hit: 2, shared_blks_read: 1, ..BufferUsage::default() };
    hi.shared_blks_hit = 7;
    hi.shared_blks_read = 4;
    hi.temp_blks_written = 3;
    buffer_usage_accum_diff(&mut a, &hi, &lo);
    assert_eq!(a.shared_blks_hit, 5);
    assert_eq!(a.shared_blks_read, 3);
    assert_eq!(a.temp_blks_written, 3);
    buffer_usage_add(&mut a, &lo);
    assert_eq!(a.shared_blks_hit, 7);

    let mut w = WalUsage::default();
    let hi = WalUsage { wal_records: 5, wal_bytes: 100, ..WalUsage::default() };
    let lo = WalUsage { wal_records: 2, wal_bytes: 30, ..WalUsage::default() };
    wal_usage_accum_diff(&mut w, &hi, &lo);
    assert_eq!(w.wal_records, 3);
    assert_eq!(w.wal_bytes, 70);
}

#[test]
fn agg_node_merges_cycles() {
    let mut dst = timed();
    let mut add = timed();
    instr_start_node(&mut add);
    instr_stop_node(&mut add, 2.0);
    instr_end_loop(&mut add);
    instr_agg_node(&mut dst, &add);
    assert_eq!(dst.ntuples, 2.0);
    assert_eq!(dst.nloops, 1.0);
}

#[test]
fn current_time_is_monotonic_nonzero() {
    let a = instr_time_current();
    let b = instr_time_current();
    assert!(!a.is_zero());
    assert!(b.ticks >= a.ticks);
}

#[test]
fn restart_while_unwinding_does_not_double_panic() {
    struct Cleanup(Instrumentation);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            instr_start_node(&mut self.0);
            instr_stop_node(&mut self.0, 0.0);
            instr_stop_node(&mut self.0, 0.0);
            instr_end_loop(&mut self.0);
        }
    }
    let r = std::panic::catch_unwind(|| {
        let mut i = timed();
        instr_start_node(&mut i);
        let _cleanup = Cleanup(i);
        panic!("runtime error mid-node");
    });
    assert!(r.is_err());
}

#[test]
#[should_panic(expected = "InstrStartNode called twice in a row")]
fn restart_outside_an_unwind_still_panics() {
    let mut i = timed();
    instr_start_node(&mut i);
    instr_start_node(&mut i);
}

#[test]
fn accum_parallel_query_folds_wal_into_the_live_counter() {
    use core::sync::atomic::{AtomicI64, Ordering::Relaxed};
    static RECORDS: AtomicI64 = AtomicI64::new(0);
    static FPI: AtomicI64 = AtomicI64::new(0);
    if !transam_xlog_seams::wal_usage_add::is_installed() {
        transam_xlog_seams::wal_usage_add::set(|w| {
            RECORDS.fetch_add(w.wal_records, Relaxed);
            FPI.fetch_add(w.wal_fpi, Relaxed);
        });
    }
    if !transam_xlog_seams::wal_usage::is_installed() {
        transam_xlog_seams::wal_usage::set(|| WalUsage {
            wal_records: RECORDS.load(Relaxed),
            wal_fpi: FPI.load(Relaxed),
            wal_bytes: 0,
            wal_buffers_full: 0,
        });
    }
    let live_before = transam_xlog_seams::wal_usage::call();
    let before = pg_wal_usage();
    instr_accum_parallel_query(
        &BufferUsage::default(),
        &WalUsage { wal_records: 3, wal_fpi: 2, wal_bytes: 0, wal_buffers_full: 0 },
    );
    let live_after = transam_xlog_seams::wal_usage::call();
    assert_eq!(live_after.wal_records - live_before.wal_records, 3);
    assert_eq!(live_after.wal_fpi - live_before.wal_fpi, 2);
    let after = pg_wal_usage();
    assert_eq!(after.wal_records - before.wal_records, 3, "no double fold through the overlay");
}
