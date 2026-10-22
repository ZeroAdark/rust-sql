//! `ops::sort::topn` (v5 serial step, landing 5: module lines written by modexit.py).
pub mod parthaving;
mod raw;
pub mod t3;
/// The distinct count top's dense index, in the family's raw half (raw/t3_dense.rs), under the path its readers name.
pub use raw::t3_dense;
pub mod twopass_top;
