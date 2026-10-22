//! `ops::group`: the group family's kernels, all in its raw half (raw/).
pub mod raw;

/// The plain one-word count(DISTINCT x)'s window host and insert-count owner (raw/entries_distinct.rs), under the path
/// their readers (lowerv4 group distinct_plain.rs / distinct_total.rs, the kernels test) name.
pub use raw::entries_distinct;
