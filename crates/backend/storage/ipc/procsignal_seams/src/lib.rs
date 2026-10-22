seam_core::seam!(
    pub fn proc_signal_barrier_pending() -> bool
);

seam_core::seam!(
    pub fn process_proc_signal_barrier() -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn drain_thread_signals() -> types_error::PgResult<()>
);

seam_core::seam!(
    // Returns 0 on success, errno on failure (ESRCH when no such backend, as C's kill()).
    pub fn send_thread_signal(pid: i32, signo: i32) -> i32
);

seam_core::seam!(
    // Raw = LatchHandle::as_usize; None clears.
    pub fn set_thread_signal_extra_wake_latch(raw_latch: Option<usize>) -> ()
);

seam_core::seam!(
    // proc_number = INVALID_PROC_NUMBER for the pid-scan path.
    pub fn send_proc_signal(
        pid: i32,
        reason: types_storage::storage::ProcSignalReason,
        proc_number: i32,
    ) -> i32
);
