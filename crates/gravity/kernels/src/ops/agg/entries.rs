//! M1's host entries (plan §4.3, the end-to-end slice): the C-ABI functions M1's lowered IR calls beyond host_abi.rs's base
//! utilities, and the layouts M1's runtime protocols hand the IR. Each entry is listed in the `hosts:` rows of the lowerv4
//! file that declares it; the keyed tables' entries and layouts are m1_keyed.rs's. Created and closed by M1 (owner M1-HOSTS).

pub use super::raw::like_frame;
