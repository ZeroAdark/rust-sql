//! `ops::sort` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod exact_feed;
pub(crate) mod inc_batches;
pub(crate) mod inc_held;
pub(crate) mod inc_replay;
pub(crate) mod inc_stream;
pub use inc_stream::stream_producers;
pub(crate) mod incremental;
pub(crate) mod lower;
pub(crate) mod raw;
pub(crate) mod shapes;
pub(crate) mod sort_noproc;
pub(crate) mod sort_raise;
pub mod strategies;
pub(crate) mod topk_rows;
pub mod topn;
