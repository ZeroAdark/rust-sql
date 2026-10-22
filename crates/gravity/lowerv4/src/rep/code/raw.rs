//! rep/code's unsafe host entries, the module's one home of unsafe code: the code order's prefix word, moved unchanged
//! from the files named on each module below, which import them back, so the addresses the IR takes, host names and IR
//! are unchanged.
#![allow(unsafe_code)]

/// `order.rs`'s entries.
pub(crate) mod order {
    gravity_kernels::contract::host! {
        /// The first 8 payload bytes of the varlena at `p`, big-endian, zero-padded, top bit flipped. # Safety — `p` is a live
        /// uncompressed varlena image (a dictionary render).
        pub(crate) unsafe fn prefix8(p: u64) -> u64 {
            let b = gravity_kernels::dict::reps::varlena_bytes(p as usize as *const u8);
            let mut w = [0u8; 8];
            let n = b.len().min(8);
            w[..n].copy_from_slice(&b[..n]);
            u64::from_be_bytes(w) ^ (1 << 63)
        }
    }
}
