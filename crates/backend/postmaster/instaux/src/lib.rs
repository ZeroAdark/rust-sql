//! The instance aux (`BackendType::InstanceAux`): mutually authenticated
//! control endpoints a lifecycle controller drives an instance through —
//! status, probes, configuration check and apply, promotion, fencing, role
//! and certificate apply. A postmaster-lifetime listener thread answers
//! reads from the last published sample; the aux process samples the
//! server, runs the liveness commit, and executes mutations one at a time.
#![allow(non_snake_case)]

// The aux runs beside a postmaster, behind TCP / Unix listeners and OpenSSL; a wasm32 build has none of them (one
// --single backend), so there the crate is its public surface alone (wasm.rs).
macro_rules! native {
    ($($item:item)*) => { $( #[cfg(not(target_family = "wasm"))] $item )* };
}

native! {
    mod admit;
    mod api;
    mod apply;
    mod config;
    mod fence;
    mod files;
    mod http;
    mod json;
    mod liveness;
    mod main_loop;
    mod probes;
    mod promote;
    mod roles;
    mod sample;
    mod server;
    mod shared;
    mod state;
    mod status;
    mod tls;

    pub use main_loop::InstanceAuxMain;
    pub use server::postmaster_start;
    pub use shared::note_pm_state;
}
mod names;
#[cfg(target_family = "wasm")]
mod wasm;

#[cfg(all(test, not(target_family = "wasm")))]
mod tests;

pub use names::{pm_state_name, PM_STATE_NAMES, STATUS_SCHEMA};
#[cfg(target_family = "wasm")]
pub use wasm::{note_pm_state, postmaster_start, InstanceAuxMain};
