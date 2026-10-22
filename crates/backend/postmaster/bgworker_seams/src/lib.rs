seam_core::seam!(
    // true = BGWH_STOPPED or BGWH_POSTMASTER_DIED
    pub fn background_worker_stopped(slot: i32, generation: u64) -> bool
);

seam_core::seam!(
    pub fn get_background_worker_type_by_pid(pid: i32) -> Option<String>
);
