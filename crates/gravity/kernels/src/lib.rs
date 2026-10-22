//! Gravity kernels, pure Rust over the ABI (docs/gravity/kernels.md): the host ABI and owner folds the server links, and the lab hand kernels (`par_*`) behind `hand-kernels`.

pub mod leader;
#[cfg(feature = "hand-kernels")]
pub mod par_distinct;
#[cfg(feature = "hand-kernels")]
pub mod par_hashagg_codes;
pub mod par_hashjoin;
#[cfg(feature = "hand-kernels")]
pub mod par_scan_count;
#[cfg(feature = "hand-kernels")]
pub mod par_sort;
pub mod par_topk;
pub mod util;
pub mod contract;
#[macro_use]
pub mod host_abi;
pub mod dict;
pub mod ops;
pub mod table;
pub mod typesem;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Touch {
    pub hdr: &'static [usize],
    pub sites: &'static [(usize, &'static [usize])],
}

/// The site words a run may change: the window cells from `sel` through `answered`'s word.
pub const SITE_EXEC_CELLS: &[usize] = {
    use gravity_abi::site::site::{ANSWER, CONT, CURSOR, GRANULE, PART_ID, POS, POS_BLOCK, ROWS_IN, ROWS_OUT, SEL, STATE};
    &[SEL, ANSWER, ROWS_IN, ROWS_OUT, CONT, STATE, CURSOR, PART_ID, GRANULE, POS, POS_BLOCK]
};
