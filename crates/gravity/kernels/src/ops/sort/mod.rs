//! `ops::sort` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod cut;
pub(crate) mod polled;
pub mod entries;
pub(crate) mod raw;
pub(crate) mod rowsort;
pub use rowsort::{restore_at, restore_thread_state, save_at, save_thread_state, trim_levels, SortThreadState};
pub mod runmerge;
pub(crate) mod runs;
pub(crate) mod topk;
pub mod topn;
