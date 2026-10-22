//! Graph properties computed once (plan §3): the struct and its walk (props.rs), split by the walk's parts (main 09-24).
//! Its module lines live here, not in the crate root (rule R9; construct bundle core-face-09).
pub(crate) mod code;
mod ops;
mod props;
pub use props::*;
pub(crate) use crate::ops::append::merge_admit::{arms_admit, incremental_admit, merge_cap, merge_pull, ops_can_raise, pure_given, Merge};
pub(crate) use crate::ops::group::regroup::fed_in_order;
