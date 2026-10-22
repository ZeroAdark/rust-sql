//! Gravity wire output: RowDescription, DataRow and the completion tag straight to the protocol (docs/gravity/wire.md).
#![deny(unsafe_code)]

pub mod completion;
pub mod datarow;
pub mod rowdesc;

pub use completion::completion;
pub use datarow::{frame_rows, WireNode};
pub use rowdesc::RowDesc;

/// The protocol send buffer's growth before its first flush when full (pqcomm `set_send_grow`), set once per process by
/// exec glue.rs from `PGRUST_GRAVITY_SEND_GROW`.
pub fn set_send_grow(on: bool) {
    // a test process with no protocol buffers installed has nothing to size
    if pqcomm_seams::set_send_grow::is_installed() {
        pqcomm_seams::set_send_grow::call(on);
    }
}
