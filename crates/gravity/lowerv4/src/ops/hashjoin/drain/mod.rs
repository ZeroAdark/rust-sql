//! `ops::hashjoin::drain` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod arms;
pub(crate) mod lower;
pub(crate) mod part;
pub(crate) mod probe_full;
pub(crate) mod probe_right;
pub(crate) mod raw;
pub mod source;
pub(crate) mod spill;
pub(crate) mod spool;
pub(crate) mod spool_admit;
pub(crate) mod spool_arms;
pub(crate) mod spool_stream;
