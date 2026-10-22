use gravity_abi::ctx::hdr;

use gravity_abi::breaker::TopK;

use crate::{Touch, SITE_EXEC_CELLS};

/// The largest LIMIT the walk serves (the run is a sorted array: insertion is O(n)).
pub const MAX_N: u32 = 1024;

pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };

pub const fn bytes(n: u32, natts: u32) -> u32 {
    TopK::bytes(n, natts)
}

#[cfg(feature = "hand-kernels")]
mod hand;
#[cfg(feature = "hand-kernels")]
pub use hand::*;
