//! Lowerer v4's host entries (docs/gravity/lowerv4/plan.md §4.3): the C-ABI functions lowered IR calls, one module per
//! milestone (`m<k>.rs`). APPEND-ONLY: a milestone adds its `pub mod m<k>;` line right above the sentinel and edits nothing
//! else here; the base utilities stay in host_abi.rs.
pub mod m7;
pub mod m6_raise;
// v4-end
