//! The postmaster state names and the status document's schema id: plain data the aux and its callers read on every
//! target, wasm32 included.

pub const PM_STATE_NAMES: [&str; 13] = [
    "init",
    "startup",
    "recovery",
    "hot_standby",
    "run",
    "stop_backends",
    "wait_backends",
    "wait_xlog_shutdown",
    "wait_xlog_archival",
    "wait_io_workers",
    "wait_dead_end",
    "wait_checkpointer",
    "no_children",
];

pub fn pm_state_name(state: u8) -> &'static str {
    PM_STATE_NAMES.get(state as usize).copied().unwrap_or("unknown")
}

pub const STATUS_SCHEMA: &str = "pgrust.instance_aux.status/v1";
