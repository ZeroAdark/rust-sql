//! `inputs` (v5 serial step, landing 5: module lines written by modexit.py).
pub mod front;
pub mod genp;
/// PostgreSQL's `get_hash_memory_limit()` and `work_mem` read at run time (memoize's Cache object, the execution's memory grant, EXPLAIN ANALYZE's memory accounts).
pub(crate) mod hash_mem;
pub mod knobs;
pub(crate) mod pgrc;
mod raw;
