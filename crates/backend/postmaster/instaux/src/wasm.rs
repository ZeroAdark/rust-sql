//! The instance aux on wasm32: there is no postmaster to start it, so `pgrust.instance_aux` is refused as a feature
//! this platform lacks, a PMState change has no reader, and the aux's main is never launched.

use types_error::{PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED, FATAL};
use types_startup::StartupData;

pub fn postmaster_start() -> PgResult<()> {
    if !guc_tables::backing::pgrust_instance_aux() {
        return Ok(());
    }
    Err(Box::new(
        PgError::new(FATAL, "pgrust.instance_aux is not supported on this platform".to_string())
            .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
    ))
}

pub fn note_pm_state(_state: u8, _fatal_error: bool) {}

pub fn InstanceAuxMain(_startup_data: &StartupData) -> ! {
    unreachable!("the instance aux is never launched on wasm32")
}
