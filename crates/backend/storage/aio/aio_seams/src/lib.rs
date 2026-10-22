seam_core::seam!(
    pub fn pgaio_closing_fd(fd: i32)
);

seam_core::seam!(
    pub fn pgaio_io_start_readv(fd: i32, iovcnt: i32, offset: i64) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn pgaio_io_release_resowner(ioh_node: usize, on_error: bool) -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn at_eoxact_aio(is_commit: bool)
);

seam_core::seam!(
    // Caller has the buffer pinned with BM_IO_IN_PROGRESS; the submit arms desc.io_wref before the SQE is visible.
    pub fn uring_buf_read(fd: i32, offset: i64, buffer: i32) -> bool
);

seam_core::seam!(
    pub fn uring_buf_read_wait(aio_index: u32, generation: u64)
);

seam_core::seam!(
    pub fn uring_collect_done(out: &mut [i32]) -> usize
);

seam_core::seam!(
    pub fn uring_drain_own(out: &mut [i32]) -> usize
);

seam_core::seam!(
    pub fn uring_available() -> bool
);

seam_core::seam!(
    pub fn uring_worker_ring_init() -> i32
);

seam_core::seam!(
    pub fn uring_worker_ring_teardown()
);

seam_core::seam!(
    pub fn uring_boundary_reap()
);

seam_core::seam!(
    pub fn io_permit_release() -> bool
);

seam_core::seam!(
    // Pairs 1:1 with true returns.
    pub fn io_permit_reacquire()
);

seam_core::seam!(
    pub fn uring_drain_all_raw()
);

seam_core::seam!(
    pub fn pgaio_error_cleanup()
);

seam_core::seam!(
    pub fn pgaio_init_backend() -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn pgaio_enter_batchmode() -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn pgaio_exit_batchmode() -> types_error::PgResult<()>
);
