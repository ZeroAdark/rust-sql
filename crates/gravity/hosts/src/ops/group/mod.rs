//! `ops::group` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod hosted_distinct;
pub(crate) mod raw;

/// The distinct fold face, in the family's raw half (raw/distinct_fold.rs), under the path its readers name.
pub use raw::distinct_fold;
