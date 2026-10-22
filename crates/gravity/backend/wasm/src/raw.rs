//! The wasm backend's one unsafe step (rule R6, its raw home): `run`, the interpreted entry `Code::interp` calls with the
//! `WasmCode` it was handed back as `state`.
#![allow(unsafe_code)]

use gravity_abi::Ctx;

use crate::embed::WasmCode;

gravity_abi::host! {
    #[unwind]
    pub(crate) unsafe fn run(state: *const (), ctx: *mut Ctx) -> i64 {
        let code = &*(state as *const WasmCode);
        // The one landing pad: an unwind is parked and `Code::call` resumes it on return.
        gravity_abi::status::unwind::pad(|| code.call(ctx)).unwrap_or(0)
    }
}
