//! rep/cstr's unsafe host entries, the module's one home of unsafe code: a cstring's size, moved unchanged from the
//! files named on each module below, which import them back, so the addresses the IR takes, host names and IR are
//! unchanged.
#![allow(unsafe_code)]

/// `datum.rs`'s entries.
pub(crate) mod datum {
    gravity_kernels::contract::host! {
        /// `strlen(p) + 1`. # Safety — `p` is a NUL-terminated string.
        pub(crate) unsafe fn cstr_size(p: u64) -> u64 {
            core::ffi::CStr::from_ptr(p as usize as *const core::ffi::c_char).to_bytes().len() as u64 + 1
        }
    }
}
