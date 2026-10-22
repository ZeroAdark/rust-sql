seam_core::seam!(
    // exitstatus uses C wait-status encoding (exit code << 8; a crash unwind reports as WTERMSIG SIGABRT).
    pub fn announce_child_exit(pid: i32, exitstatus: i32)
);

seam_core::seam!(
    pub fn signal_postmaster_sigusr1()
);

seam_core::seam!(
    pub fn signal_postmaster_sighup()
);

seam_core::seam!(
    pub fn pg_start_time() -> i64
);

seam_core::seam!(
    pub fn set_pg_start_time(ts: i64)
);

seam_core::seam!(
    // Called under the bgworker registry lock; lock order registry -> pool -> pmchild matches BackgroundWorkerStateChange's registry -> pmchild.
    pub fn parallel_pool_dispatch(slot: i32, generation: u64, dboid: types_core::Oid) -> i32
);

seam_core::seam!(
    pub fn parallel_pool_retire_db(dboid: types_core::Oid)
);

seam_core::seam!(
    pub fn wpool_maintain()
);

seam_core::seam!(
    pub fn wpool_flush()
);

seam_core::seam!(
    pub fn wpool_population() -> i32
);

seam_core::seam!(
    pub fn rtpool_start() -> i32
);

seam_core::seam!(
    pub fn rtpool_stop()
);

seam_core::seam!(
    pub fn rtpool_population() -> i32
);

seam_core::seam!(
    pub fn rtgang_procs_wanted() -> i32
);

seam_core::seam!(
    pub fn rtgang_install()
);

seam_core::seam!(
    pub fn rtgang_retire()
);

seam_core::seam!(
    pub fn rtgang_live() -> i32
);

seam_core::seam!(
    pub fn bgworker_shmem_init() -> types_error::PgResult<()>
);

seam_core::seam!(
    pub fn pm_promote_run()
);

seam_core::seam!(
    pub fn pm_service_pending()
);
